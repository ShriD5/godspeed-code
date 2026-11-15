//! The UI half of the `preview_*` agent tools: the scripts an action runs in
//! the page (`agent.js`), what comes back, and the screenshot encoding. No
//! native handles; the shell drives a tab through [`super::BrowserSurface`].

use serde_json::{Value, json};
use zeron_proto::preview_agent::{
    PREVIEW_CONSOLE_MAX, PREVIEW_EVAL_MAX_BYTES, PREVIEW_SCREENSHOT_MAX_SIDE,
    PREVIEW_SNAPSHOT_MAX_BYTES, PREVIEW_SNAPSHOT_MAX_NODES, PreviewAction, cap_text,
};

/// The page-side helper. Also the document-start script of agent tabs, so
/// console messages are captured from the first line of the page.
pub const AGENT_SCRIPT: &str = include_str!("agent.js");

/// The script that runs one in-page action and returns its JSON string.
/// `None` for actions the page doesn't run (open, navigate, screenshot).
pub fn action_script(action: &PreviewAction) -> Option<String> {
    let limits = json!({
        "maxNodes": PREVIEW_SNAPSHOT_MAX_NODES,
        "maxBytes": PREVIEW_SNAPSHOT_MAX_BYTES,
        "maxEvalBytes": PREVIEW_EVAL_MAX_BYTES,
        "maxConsole": PREVIEW_CONSOLE_MAX,
    });
    let (payload, thunk) = match action {
        PreviewAction::Open { .. } | PreviewAction::Navigate { .. } | PreviewAction::Screenshot => {
            return None;
        }
        // The expression is code by design: compiled into the script as a
        // thunk (never through `eval`, which a page's CSP may forbid). Its
        // text is not repeated in the payload.
        PreviewAction::Eval { expression } => (
            json!({ "action": "eval" }),
            format!("() => ({}\n)", expression.trim()),
        ),
        other => (serde_json::to_value(other).ok()?, "undefined".to_owned()),
    };
    Some(format!(
        "{AGENT_SCRIPT}\n;window.__godspeedAgent.run({payload}, {limits}, {thunk})"
    ))
}

/// The page's state probe, for a wait between steps.
pub fn state_script() -> String {
    format!(
        "{AGENT_SCRIPT}\n;window.__godspeedAgent.run({{\"action\":\"state\"}}, {{}}, undefined)"
    )
}

/// Read what an action's script returned.
pub fn parse_result(raw: Result<String, String>) -> Result<Value, String> {
    let text = raw.map_err(|error| format!("The page could not run the action: {error}"))?;
    let value: Value = serde_json::from_str(&text)
        .map_err(|_| "The page returned something unreadable".to_owned())?;
    if let Some(err) = value.get("err") {
        return Err(err.as_str().unwrap_or("The page reported an error").to_owned());
    }
    value
        .get("ok")
        .cloned()
        .ok_or_else(|| "The page returned no result".to_owned())
}

/// Hold the snapshot to its cap even if the page's helper didn't.
pub fn finish_snapshot(mut value: Value) -> Value {
    if let Some(outline) = value.get("outline").and_then(Value::as_str) {
        let (outline, cut) = cap_text(outline, PREVIEW_SNAPSHOT_MAX_BYTES);
        value["outline"] = Value::String(outline);
        if cut {
            value["truncated"] = Value::Bool(true);
        }
    }
    for key in ["title", "url"] {
        if let Some(text) = value.get(key).and_then(Value::as_str) {
            let (text, _) = cap_text(text, 2048);
            value[key] = Value::String(text);
        }
    }
    value
}

/// Decode a native snapshot (any format `image` reads; WebKit hands back
/// TIFF), fit it within [`PREVIEW_SCREENSHOT_MAX_SIDE`], and return base64
/// PNG with its size.
pub fn encode_screenshot(bytes: &[u8]) -> Result<(String, u32, u32), String> {
    use base64::Engine as _;
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| format!("unreadable screenshot: {e}"))?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    limits.max_alloc = Some(512 * 1024 * 1024);
    reader.limits(limits);
    let image = reader
        .decode()
        .map_err(|e| format!("unreadable screenshot: {e}"))?;
    let (width, height) = (image.width(), image.height());
    if width == 0 || height == 0 {
        return Err("the preview has no visible area to capture".into());
    }
    let max = PREVIEW_SCREENSHOT_MAX_SIDE;
    let image = if width > max || height > max {
        image.resize(max, max, image::imageops::FilterType::Triangle)
    } else {
        image
    };
    let image = image::DynamicImage::ImageRgba8(image.to_rgba8());
    let mut png = std::io::Cursor::new(Vec::new());
    image
        .write_to(&mut png, image::ImageFormat::Png)
        .map_err(|e| format!("could not encode the screenshot: {e}"))?;
    Ok((
        base64::engine::general_purpose::STANDARD.encode(png.into_inner()),
        image.width(),
        image.height(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_page_actions_carry_their_payload_and_limits() {
        let script = action_script(&PreviewAction::Click {
            reference: Some("e7".into()),
            selector: None,
        })
        .unwrap();
        assert!(script.starts_with(AGENT_SCRIPT));
        assert!(script.contains(r#"{"action":"click","ref":"e7"}"#), "{script}");
        assert!(script.contains("\"maxBytes\":24576"));
        assert!(action_script(&PreviewAction::Screenshot).is_none());
        assert!(
            action_script(&PreviewAction::Open {
                url: "http://localhost:3000".into()
            })
            .is_none()
        );
    }

    #[test]
    fn eval_compiles_the_expression_as_a_thunk() {
        let script = action_script(&PreviewAction::Eval {
            expression: " document.title // trailing comment".into(),
        })
        .unwrap();
        // The newline keeps a trailing comment from swallowing the paren.
        assert!(script.ends_with("() => (document.title // trailing comment\n))"));
        assert!(script.contains(r#"run({"action":"eval"}"#));
    }

    #[test]
    fn results_and_errors_from_the_page() {
        assert_eq!(
            parse_result(Ok(r#"{"ok":{"clicked":"button \"Save\""}}"#.into())).unwrap()["clicked"],
            "button \"Save\""
        );
        assert_eq!(
            parse_result(Ok(r#"{"err":"no element matches #x"}"#.into())).unwrap_err(),
            "no element matches #x"
        );
        assert!(parse_result(Ok("not json".into())).is_err());
        assert!(
            parse_result(Err("SyntaxError: Unexpected token".into()))
                .unwrap_err()
                .contains("SyntaxError")
        );
    }

    #[test]
    fn snapshots_are_held_to_the_cap() {
        let outline = (0..5000)
            .map(|i| format!("button \"Item {i}\" [ref=e{i}]"))
            .collect::<Vec<_>>()
            .join("\n");
        let value = finish_snapshot(json!({"title": "T", "url": "http://localhost/", "outline": outline}));
        assert!(value["outline"].as_str().unwrap().len() <= PREVIEW_SNAPSHOT_MAX_BYTES);
        assert_eq!(value["truncated"], true);
        let small = finish_snapshot(json!({"outline": "heading \"Hi\"", "truncated": false}));
        assert_eq!(small["truncated"], false);
    }

    #[test]
    fn screenshots_are_downscaled_pngs() {
        use base64::Engine as _;
        let big = image::RgbaImage::from_pixel(2560, 1600, image::Rgba([40, 70, 60, 255]));
        let mut tiff = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(big)
            .write_to(&mut tiff, image::ImageFormat::Tiff)
            .unwrap();
        let (png, width, height) = encode_screenshot(&tiff.into_inner()).unwrap();
        assert_eq!((width, height), (1280, 800));
        let bytes = base64::engine::general_purpose::STANDARD.decode(png).unwrap();
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
        assert!(encode_screenshot(b"nope").is_err());
    }
}
