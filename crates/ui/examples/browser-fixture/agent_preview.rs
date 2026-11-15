//! Agent preview tools end to end in native WebKit: each `preview_*` action
//! runs through the window's real driver path (`perform`) on the fixture
//! chat's agent tab, against the loopback `/agent` page. No engine: the
//! requests are the ones the engine would forward after its policy checks.
use super::*;
use gpui::WindowHandle;
use serde_json::Value;
use zeron_proto::preview_agent::{PreviewAction, PreviewDriverRequest};

const CHAT: &str = "browser-fixture";

async fn act(
    window: WindowHandle<shell::Shell>,
    action: PreviewAction,
    cx: &mut AsyncApp,
) -> anyhow::Result<Result<Value, String>> {
    let request = PreviewDriverRequest {
        id: uuid::Uuid::new_v4().to_string(),
        chat_id: CHAT.into(),
        action,
    };
    let task = window.update(cx, |shell, w, cx| shell.fixture_agent_preview(request, w, cx))?;
    Ok(task.await)
}

async fn ok(
    window: WindowHandle<shell::Shell>,
    action: PreviewAction,
    cx: &mut AsyncApp,
) -> anyhow::Result<Value> {
    let tool = action.tool();
    act(window, action, cx)
        .await?
        .map_err(|error| anyhow::anyhow!("{tool} failed: {error}"))
}

/// The ref on the outline line that contains `needle`.
fn reference(outline: &str, needle: &str) -> anyhow::Result<String> {
    let line = outline
        .lines()
        .find(|line| line.contains(needle))
        .ok_or_else(|| anyhow::anyhow!("no {needle:?} in the snapshot:\n{outline}"))?;
    let start = line
        .find("[ref=")
        .ok_or_else(|| anyhow::anyhow!("{needle:?} has no ref: {line}"))?
        + 5;
    let end = line[start..]
        .find(']')
        .ok_or_else(|| anyhow::anyhow!("unterminated ref: {line}"))?;
    Ok(line[start..start + end].to_owned())
}

pub async fn exercise(
    window: WindowHandle<shell::Shell>,
    origin: &str,
    output: &std::path::Path,
    cx: &mut AsyncApp,
) -> anyhow::Result<()> {
    // No tab yet: navigate refuses, open creates the chat's agent tab.
    let refused = act(
        window,
        PreviewAction::Navigate {
            url: format!("{origin}/agent"),
        },
        cx,
    )
    .await?;
    anyhow::ensure!(refused.is_err(), "navigate worked without a tab");

    let opened = ok(
        window,
        PreviewAction::Open {
            url: format!("{origin}/agent"),
        },
        cx,
    )
    .await?;
    anyhow::ensure!(opened["title"] == "Greeter", "open: {opened}");
    let tab = window
        .update(cx, |shell, _, _| shell.fixture_agent_tab(CHAT))?
        .ok_or_else(|| anyhow::anyhow!("no agent tab recorded"))?;
    anyhow::ensure!(
        tab.read_with(cx, |b, _| b.agent_owned() && b.agent_using()),
        "the tab does not show agent use"
    );

    let snapshot = ok(window, PreviewAction::Snapshot, cx).await?;
    let outline = snapshot["outline"].as_str().unwrap_or_default().to_owned();
    std::fs::write(output.join("agent-snapshot.txt"), &outline)?;
    anyhow::ensure!(outline.contains("heading \"Greeter\""), "{outline}");
    let field = reference(&outline, "textbox \"Name\"")?;
    let button = reference(&outline, "button \"Greet\"")?;

    ok(
        window,
        PreviewAction::Type {
            reference: Some(field),
            selector: None,
            text: "Ada".into(),
            submit: false,
        },
        cx,
    )
    .await?;
    let clicked = ok(
        window,
        PreviewAction::Click {
            reference: Some(button),
            selector: None,
        },
        cx,
    )
    .await?;
    anyhow::ensure!(
        clicked["clicked"].as_str().is_some_and(|c| c.contains("Greet")),
        "click: {clicked}"
    );
    let waited = ok(
        window,
        PreviewAction::Wait {
            selector: None,
            text: Some("Hello, Ada".into()),
            timeout_ms: Some(5_000),
        },
        cx,
    )
    .await?;
    anyhow::ensure!(waited["found"] == true, "wait: {waited}");

    let evaluated = ok(
        window,
        PreviewAction::Eval {
            expression: "document.getElementById('out').textContent".into(),
        },
        cx,
    )
    .await?;
    anyhow::ensure!(evaluated["value"] == "Hello, Ada", "eval: {evaluated}");
    // Cookies stay hidden while an expression runs, even past the word filter.
    let hidden = act(
        window,
        PreviewAction::Eval {
            expression: "Object.getOwnPropertyDescriptor(Document.prototype, 'coo' + 'kie').get.call(document)".into(),
        },
        cx,
    )
    .await?;
    anyhow::ensure!(hidden.is_err(), "cookie getter was reachable: {hidden:?}");

    let console = ok(window, PreviewAction::Console { clear: false }, cx).await?;
    let text = console.to_string();
    anyhow::ensure!(
        text.contains("agent fixture ready") && text.contains("greeted Ada"),
        "console: {console}"
    );

    let scrolled = ok(
        window,
        PreviewAction::Scroll {
            reference: None,
            selector: None,
            dx: 0,
            dy: 0,
            to: Some("bottom".into()),
        },
        cx,
    )
    .await?;
    anyhow::ensure!(scrolled.get("y").is_some(), "scroll: {scrolled}");

    let shot = ok(window, PreviewAction::Screenshot, cx).await?;
    let width = shot["width"].as_u64().unwrap_or(0);
    anyhow::ensure!(width > 0 && width <= 1280, "screenshot: width {width}");
    {
        use base64::Engine as _;
        let png = base64::engine::general_purpose::STANDARD
            .decode(shot["png"].as_str().unwrap_or_default())?;
        std::fs::write(output.join("agent-screenshot.png"), &png)?;
        image::load_from_memory(&png)?;
    }

    // Stop takes the tab back: the next action refuses.
    tab.update(cx, |b, cx| b.stop_agent(cx));
    pause(cx, 100).await;
    let stopped = act(window, PreviewAction::Snapshot, cx).await?;
    anyhow::ensure!(
        stopped.as_ref().is_err_and(|e| e.contains("stopped")),
        "stop did not revoke control: {stopped:?}"
    );

    std::fs::write(
        output.join("result.txt"),
        "PASS: agent preview tools in native WebKit: open, snapshot refs, type, click, wait, eval (cookies hidden), console, scroll, screenshot, Stop.\n",
    )?;
    Ok(())
}
