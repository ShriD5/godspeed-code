//! Agents driving the in-app preview browser (`preview_*` MCP tools).
//!
//! The browser lives in the desktop UI process, so a tool call travels
//! MCP server → engine (`PreviewAct`) → the attached UI (`WatchPreviewDriver`
//! stream) → back (`AnswerPreviewRequest`). This module holds what all three
//! agree on: the action shapes, their limits, and the URL policy.

use serde::{Deserialize, Serialize};

/// The error every preview tool returns when no desktop window is attached
/// to the engine (a headless engine, or the app is closed).
pub const PREVIEW_NEEDS_WINDOW: &str = "Preview tools need the Godspeed window open";

/// Cap on a `preview_snapshot` outline, in bytes.
pub const PREVIEW_SNAPSHOT_MAX_BYTES: usize = 24 * 1024;
/// Cap on the elements an outline lists.
pub const PREVIEW_SNAPSHOT_MAX_NODES: usize = 600;
/// Cap on a `preview_eval` result, in bytes of JSON.
pub const PREVIEW_EVAL_MAX_BYTES: usize = 16 * 1024;
/// Cap on a `preview_eval` expression, in bytes.
pub const PREVIEW_EXPRESSION_MAX_BYTES: usize = 8 * 1024;
/// Cap on `preview_type` text, in bytes.
pub const PREVIEW_TEXT_MAX_BYTES: usize = 8 * 1024;
/// Cap on a selector or wait text, in bytes.
pub const PREVIEW_SELECTOR_MAX_BYTES: usize = 1024;
/// Console messages `preview_console` returns at most (newest last).
pub const PREVIEW_CONSOLE_MAX: usize = 50;
/// Longest side of a `preview_screenshot` image, in pixels.
pub const PREVIEW_SCREENSHOT_MAX_SIDE: u32 = 1280;
/// Default and ceiling for `preview_wait`.
pub const PREVIEW_WAIT_DEFAULT_MS: u64 = 5_000;
pub const PREVIEW_WAIT_MAX_MS: u64 = 30_000;
/// Largest scroll step in either direction, in CSS pixels.
pub const PREVIEW_SCROLL_MAX: i32 = 20_000;

/// The element an action targets: a `ref` from the last `preview_snapshot`,
/// or a CSS selector. Exactly one.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreviewTarget {
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector: Option<String>,
}

impl PreviewTarget {
    pub fn validate(&self) -> Result<(), String> {
        let reference = self
            .reference
            .as_deref()
            .map(str::trim)
            .filter(|r| !r.is_empty());
        let selector = self
            .selector
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        match (reference, selector) {
            (Some(_), Some(_)) => Err("pass either ref or selector, not both".into()),
            (None, None) => Err("pass a ref from preview_snapshot or a CSS selector".into()),
            (Some(reference), None) => {
                let valid = reference.len() <= 16
                    && reference.starts_with('e')
                    && reference[1..].chars().all(|c| c.is_ascii_digit())
                    && reference.len() > 1;
                if valid {
                    Ok(())
                } else {
                    Err(format!(
                        "ref {reference:?} is not a snapshot ref (like \"e12\")"
                    ))
                }
            }
            (None, Some(selector)) => bounded("selector", selector, PREVIEW_SELECTOR_MAX_BYTES),
        }
    }

    fn optional_validate(target: &Option<Self>) -> Result<(), String> {
        match target {
            Some(target) if target.reference.is_some() || target.selector.is_some() => {
                target.validate()
            }
            _ => Ok(()),
        }
    }
}

/// One thing an agent asks the preview to do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "camelCase")]
pub enum PreviewAction {
    /// Open the chat's preview tab (reusing it when it exists) at `url`.
    Open { url: String },
    /// Load `url` in the chat's existing preview tab.
    Navigate { url: String },
    /// A compact outline of the page with element refs.
    Snapshot,
    /// A PNG of the visible page, downscaled.
    Screenshot,
    Click {
        #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
        reference: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        selector: Option<String>,
    },
    Type {
        #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
        reference: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        selector: Option<String>,
        text: String,
        #[serde(default)]
        submit: bool,
    },
    /// Scroll the page, or one element, by a delta or to an edge.
    Scroll {
        #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
        reference: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        selector: Option<String>,
        #[serde(default)]
        dx: i32,
        #[serde(default)]
        dy: i32,
        /// `top` or `bottom`; overrides the deltas.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        to: Option<String>,
    },
    /// Evaluate a read-only JavaScript expression; the result as JSON.
    Eval { expression: String },
    /// Recent console messages and uncaught errors.
    Console {
        #[serde(default)]
        clear: bool,
    },
    /// Wait until `selector` matches or `text` appears.
    Wait {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        selector: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        #[serde(default, rename = "timeoutMs", skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u64>,
    },
}

impl PreviewAction {
    /// The tool name an agent called for this action.
    pub fn tool(&self) -> &'static str {
        match self {
            Self::Open { .. } => "preview_open",
            Self::Navigate { .. } => "preview_navigate",
            Self::Snapshot => "preview_snapshot",
            Self::Screenshot => "preview_screenshot",
            Self::Click { .. } => "preview_click",
            Self::Type { .. } => "preview_type",
            Self::Scroll { .. } => "preview_scroll",
            Self::Eval { .. } => "preview_eval",
            Self::Console { .. } => "preview_console",
            Self::Wait { .. } => "preview_wait",
        }
    }

    /// The address an action loads, for the URL policy.
    pub fn url(&self) -> Option<&str> {
        match self {
            Self::Open { url } | Self::Navigate { url } => Some(url),
            _ => None,
        }
    }

    /// Input the page sees as a user's: rate-limited.
    pub fn is_input(&self) -> bool {
        matches!(
            self,
            Self::Click { .. } | Self::Type { .. } | Self::Scroll { .. }
        )
    }

    /// The element target, for actions that take one.
    pub fn target(&self) -> Option<PreviewTarget> {
        match self {
            Self::Click {
                reference,
                selector,
            }
            | Self::Type {
                reference,
                selector,
                ..
            }
            | Self::Scroll {
                reference,
                selector,
                ..
            } => Some(PreviewTarget {
                reference: reference.clone(),
                selector: selector.clone(),
            }),
            _ => None,
        }
    }

    /// The wait's timeout, clamped.
    pub fn wait_timeout_ms(&self) -> u64 {
        match self {
            Self::Wait { timeout_ms, .. } => timeout_ms
                .unwrap_or(PREVIEW_WAIT_DEFAULT_MS)
                .clamp(100, PREVIEW_WAIT_MAX_MS),
            _ => 0,
        }
    }

    /// Check the shape and the limits. URLs are checked separately
    /// ([`preview_url`]) because their policy depends on the chat.
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Open { url } | Self::Navigate { url } => preview_url(url).map(|_| ()),
            Self::Snapshot | Self::Screenshot | Self::Console { .. } => Ok(()),
            Self::Click { .. } => self.target().unwrap_or_default().validate(),
            Self::Type { text, .. } => {
                self.target().unwrap_or_default().validate()?;
                if text.len() > PREVIEW_TEXT_MAX_BYTES {
                    return Err(format!(
                        "text is longer than {PREVIEW_TEXT_MAX_BYTES} bytes"
                    ));
                }
                Ok(())
            }
            Self::Scroll { dx, dy, to, .. } => {
                PreviewTarget::optional_validate(&self.target())?;
                if let Some(to) = to
                    && !matches!(to.as_str(), "top" | "bottom")
                {
                    return Err(format!("to must be \"top\" or \"bottom\", not {to:?}"));
                }
                if dx.unsigned_abs() > PREVIEW_SCROLL_MAX as u32
                    || dy.unsigned_abs() > PREVIEW_SCROLL_MAX as u32
                {
                    return Err(format!(
                        "scroll at most {PREVIEW_SCROLL_MAX} pixels at a time"
                    ));
                }
                if to.is_none() && *dx == 0 && *dy == 0 {
                    return Err("pass dy (positive scrolls down), dx, or to".into());
                }
                Ok(())
            }
            Self::Eval { expression } => {
                let expression = expression.trim();
                if expression.is_empty() {
                    return Err("expression is empty".into());
                }
                bounded("expression", expression, PREVIEW_EXPRESSION_MAX_BYTES)?;
                eval_policy(expression)
            }
            Self::Wait { selector, text, .. } => {
                let selector = selector.as_deref().map(str::trim).filter(|s| !s.is_empty());
                let text = text.as_deref().map(str::trim).filter(|s| !s.is_empty());
                match (selector, text) {
                    (None, None) => Err("pass a selector or text to wait for".into()),
                    (selector, text) => {
                        if let Some(selector) = selector {
                            bounded("selector", selector, PREVIEW_SELECTOR_MAX_BYTES)?;
                        }
                        if let Some(text) = text {
                            bounded("text", text, PREVIEW_SELECTOR_MAX_BYTES)?;
                        }
                        Ok(())
                    }
                }
            }
        }
    }
}

fn bounded(what: &str, value: &str, max: usize) -> Result<(), String> {
    if value.len() > max {
        Err(format!("{what} is longer than {max} bytes"))
    } else {
        Ok(())
    }
}

/// Words a read-only expression has no business using: storage and cookie
/// reads (no export), writes that outlive the call (timers, promises,
/// network), and ways to build code from strings. Best effort, on top of
/// the page-side guard that hides cookies and storage while it runs; the
/// page is untrusted either way.
const EVAL_BLOCKED: &[&str] = &[
    "cookie",
    "Storage",
    "indexedDB",
    "caches",
    "CookieStore",
    "cookieStore",
    "fetch",
    "XMLHttpRequest",
    "WebSocket",
    "EventSource",
    "sendBeacon",
    "setTimeout",
    "setInterval",
    "requestAnimationFrame",
    "queueMicrotask",
    "Promise",
    "async",
    "await",
    "import",
    "eval",
    "Function",
    "Worker",
    "postMessage",
    "location",
    "open(",
    "submit(",
    "requestSubmit",
    "click(",
    "remove(",
    "innerHTML",
    "outerHTML",
    "insertAdjacent",
    "appendChild",
    "replaceChildren",
    "document.write",
];

/// Refuse an eval expression that reaches for storage, the network, timers,
/// or obvious DOM writes. Assignments are refused too (`=` that isn't a
/// comparison or arrow), so `preview_eval` stays a reader.
pub fn eval_policy(expression: &str) -> Result<(), String> {
    for word in EVAL_BLOCKED {
        if expression.contains(word) {
            return Err(format!(
                "preview_eval is read-only: {word:?} is not allowed (no storage, cookies, network, timers or page changes). Use preview_click / preview_type to act."
            ));
        }
    }
    let bytes = expression.as_bytes();
    // Quoted strings ('...' and "...") are data: `[type=submit]` is not an
    // assignment. Template literals stay code, for their `${}`.
    let mut quote: Option<u8> = None;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate() {
        if let Some(open) = quote {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == open {
                quote = None;
            }
            continue;
        }
        if b == b'\'' || b == b'"' {
            quote = Some(b);
            continue;
        }
        let prev = if i > 0 { bytes[i - 1] } else { b' ' };
        let next = bytes.get(i + 1).copied().unwrap_or(b' ');
        let assignment = b == b'='
            && next != b'='
            && next != b'>'
            && !matches!(prev, b'=' | b'!' | b'<' | b'>');
        let increment = (b == b'+' && next == b'+') || (b == b'-' && next == b'-');
        if assignment || increment {
            return Err(
                "preview_eval is read-only: assignments are not allowed. Use preview_click / preview_type to act."
                    .into(),
            );
        }
    }
    Ok(())
}

/// A checked preview address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewUrl {
    /// The normalized URL to load.
    pub url: String,
    /// `scheme://host[:port]`, the unit network approvals cover.
    pub origin: String,
    /// A loopback dev server (`localhost`, `*.localhost`, `127.0.0.0/8`, `[::1]`).
    pub local: bool,
}

/// The URL policy: http(s) only, no credentials, no `file:` or browser-
/// internal schemes. A bare `localhost:3000` reads as `http://`.
pub fn preview_url(input: &str) -> Result<PreviewUrl, String> {
    let text = input.trim();
    if text.is_empty() {
        return Err("url is empty".into());
    }
    if text.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err("url contains spaces or control characters".into());
    }
    if text.contains('\\') {
        return Err("url contains a backslash".into());
    }
    let lower = text.to_ascii_lowercase();
    let has_scheme = lower.contains("://");
    let candidate = if has_scheme {
        text.to_owned()
    } else {
        // `localhost:3000/x` and `127.0.0.1:5173` are hosts, not schemes.
        let authority = lower.split(['/', '?', '#']).next().unwrap_or(&lower);
        let host_port = authority
            .rsplit_once(':')
            .is_some_and(|(_, port)| !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()));
        if lower.contains(':') && !host_port && !lower.starts_with('[') {
            return Err(blocked_scheme(&lower));
        }
        let host = authority
            .rsplit_once(':')
            .filter(|_| host_port)
            .map_or(authority, |(host, _)| host);
        let local = loopback_host(host.trim_start_matches('[').trim_end_matches(']'));
        format!("{}://{text}", if local { "http" } else { "https" })
    };
    let parsed = url::Url::parse(&candidate).map_err(|_| format!("not a valid URL: {text}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(blocked_scheme(parsed.scheme()));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("use a URL without an embedded username or password".into());
    }
    let Some(host) = parsed.host_str() else {
        return Err("url has no host".into());
    };
    let local = match parsed.host() {
        Some(url::Host::Domain(domain)) => loopback_host(domain),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    };
    let origin = match parsed.port() {
        Some(port) => format!("{}://{host}:{port}", parsed.scheme()),
        None => format!("{}://{host}", parsed.scheme()),
    };
    Ok(PreviewUrl {
        url: parsed.into(),
        origin,
        local,
    })
}

fn loopback_host(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    if host == "localhost" || host.ends_with(".localhost") {
        return true;
    }
    host.parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_loopback())
}

fn blocked_scheme(scheme: &str) -> String {
    let scheme = scheme.split(':').next().unwrap_or(scheme);
    format!(
        "only http and https URLs open in the preview ({scheme}: is blocked; no file, data, javascript or browser-internal pages)"
    )
}

/// One request the engine sends the attached UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewDriverRequest {
    pub id: String,
    pub chat_id: String,
    pub action: PreviewAction,
}

/// The UI's answer: `ok` with the action's result, or `err` with a message
/// the agent reads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewDriverAnswer {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ok: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub err: Option<String>,
}

/// `PreviewAct` params: the MCP server's call on behalf of its chat.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewActParams {
    pub chat_id: String,
    pub action: PreviewAction,
}

/// Cut `text` to at most `max` bytes on a char boundary, marking the cut.
pub fn cap_text(text: &str, max: usize) -> (String, bool) {
    if text.len() <= max {
        return (text.to_owned(), false);
    }
    const MARK: &str = "\n… (truncated)";
    let mut cut = max.saturating_sub(MARK.len());
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    // Prefer ending on a whole line.
    if let Some(newline) = text[..cut].rfind('\n')
        && newline > cut / 2
    {
        cut = newline;
    }
    (format!("{}{MARK}", &text[..cut]), true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn actions_round_trip_with_tool_shaped_fields() {
        let action: PreviewAction = serde_json::from_value(json!({
            "action": "type", "ref": "e4", "text": "hello", "submit": true
        }))
        .unwrap();
        assert_eq!(
            action,
            PreviewAction::Type {
                reference: Some("e4".into()),
                selector: None,
                text: "hello".into(),
                submit: true
            }
        );
        assert_eq!(action.tool(), "preview_type");
        assert!(action.is_input());
        let wait: PreviewAction =
            serde_json::from_value(json!({"action": "wait", "text": "Done", "timeoutMs": 90000}))
                .unwrap();
        assert_eq!(wait.wait_timeout_ms(), PREVIEW_WAIT_MAX_MS);
        assert_eq!(
            serde_json::to_value(&PreviewAction::Snapshot).unwrap(),
            json!({"action": "snapshot"})
        );
    }

    #[test]
    fn targets_need_exactly_one_well_formed_ref_or_selector() {
        let click = |reference: Option<&str>, selector: Option<&str>| PreviewAction::Click {
            reference: reference.map(Into::into),
            selector: selector.map(Into::into),
        };
        assert!(click(Some("e12"), None).validate().is_ok());
        assert!(click(None, Some("#save")).validate().is_ok());
        assert!(click(None, None).validate().is_err());
        assert!(click(Some("e1"), Some("#a")).validate().is_err());
        assert!(click(Some("button"), None).validate().is_err());
        assert!(click(Some("e"), None).validate().is_err());
        assert!(click(None, Some(&"a".repeat(2000))).validate().is_err());
    }

    #[test]
    fn scroll_wait_and_type_limits() {
        let scroll = |dy, to: Option<&str>| PreviewAction::Scroll {
            reference: None,
            selector: None,
            dx: 0,
            dy,
            to: to.map(Into::into),
        };
        assert!(scroll(400, None).validate().is_ok());
        assert!(scroll(0, Some("bottom")).validate().is_ok());
        assert!(scroll(0, None).validate().is_err());
        assert!(scroll(0, Some("left")).validate().is_err());
        assert!(scroll(50_000, None).validate().is_err());
        let wait = PreviewAction::Wait {
            selector: None,
            text: None,
            timeout_ms: None,
        };
        assert!(wait.validate().is_err());
        let typed = PreviewAction::Type {
            reference: Some("e1".into()),
            selector: None,
            text: "x".repeat(PREVIEW_TEXT_MAX_BYTES + 1),
            submit: false,
        };
        assert!(typed.validate().is_err());
    }

    #[test]
    fn urls_allow_web_and_dev_servers_only() {
        for (input, url, local) in [
            ("localhost:3000", "http://localhost:3000/", true),
            ("http://127.0.0.1:5173/a?b=1", "http://127.0.0.1:5173/a?b=1", true),
            ("[::1]:8080", "http://[::1]:8080/", true),
            ("app.localhost:4000", "http://app.localhost:4000/", true),
            ("https://example.com/docs", "https://example.com/docs", false),
            ("example.com", "https://example.com/", false),
            ("http://192.168.1.4:3000", "http://192.168.1.4:3000/", false),
        ] {
            let checked = preview_url(input).unwrap_or_else(|e| panic!("{input}: {e}"));
            assert_eq!(checked.url, url, "{input}");
            assert_eq!(checked.local, local, "{input}");
        }
        assert_eq!(
            preview_url("localhost:3000/x").unwrap().origin,
            "http://localhost:3000"
        );
        for blocked in [
            "",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "data:text/html,hi",
            "about:blank",
            "chrome://settings",
            "view-source:http://localhost:3000",
            "blob:http://localhost/1",
            "godspeed://open/chat/x",
            "ftp://example.com",
            "https://user:pw@example.com",
            "http://local host",
            "http://localhost:3000\\x",
        ] {
            assert!(preview_url(blocked).is_err(), "{blocked:?} passed");
        }
        assert!(
            preview_url("file:///tmp/a")
                .unwrap_err()
                .contains("file: is blocked")
        );
    }

    #[test]
    fn eval_stays_a_reader() {
        for ok in [
            "document.title",
            "[...document.querySelectorAll('li')].map(li => li.textContent)",
            "document.querySelectorAll('.row').length >= 3",
            "getComputedStyle(document.body).color",
            "a !== b && c <= d",
            "document.querySelector('input[type=email]').value",
            "document.querySelector(\"[data-x='a=b']\") !== null",
        ] {
            assert!(eval_policy(ok).is_ok(), "{ok}");
        }
        for blocked in [
            "document.cookie",
            "localStorage.getItem('token')",
            "window['session' + 'Storage']",
            "fetch('/api')",
            "document.body.innerHTML = ''",
            "x = 1",
            "i++",
            "`${x = 1}`",
            "location.href",
            "setTimeout(() => 1)",
            "new Promise(r => r(1))",
            "document.querySelector('form').submit()",
        ] {
            assert!(eval_policy(blocked).is_err(), "{blocked}");
        }
    }

    #[test]
    fn cap_text_cuts_on_a_line_and_marks_it() {
        let text = (0..100).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let (capped, cut) = cap_text(&text, 200);
        assert!(cut);
        assert!(capped.len() <= 200);
        assert!(capped.ends_with("… (truncated)"));
        assert_eq!(cap_text("short", 200), ("short".into(), false));
        let (multi, _) = cap_text(&"é".repeat(300), 101);
        assert!(multi.len() <= 101);
    }
}
