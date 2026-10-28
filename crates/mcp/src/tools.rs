//! The tool catalog and its dispatch.
//!
//! Every tool is a thin composition of engine reads/writes from
//! [`Godspeed`]; the only logic that lives here is argument resolution (chat
//! by prefix, project by path), sender attribution, and the "how do I
//! deliver a message to a chat in this state" choice the composer makes
//! for humans.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use zeron_doc::SessionCommandPayload;
use zeron_proto::{
    AccessMode, ApprovalDecision, ApprovalRisk, Chat, ChatConfig, HarnessId, MAX_CONDUCTOR_DEPTH,
    MAX_NOTES_BYTES, ReasoningLevel, RunRequest, SandboxLevel, Session, SessionStatus, Space,
    UserInputAnswer, UserInputQuestion, approval_risk, can_spawn_at,
};

use crate::godspeed::{Godspeed, HarnessInfo, TurnOutcome, session_for, short};
use crate::transcript::{RenderOptions, RenderedMessage, render_entries};

/// Default and ceiling for the blocking waits.
const MAX_BATCH: usize = 32;
const DEFAULT_WAIT: Duration = Duration::from_secs(600);
const MAX_WAIT: Duration = Duration::from_secs(3600);
/// A session row older than this is not trusted to still be working
/// (the UI's staleness window): a crashed host must not read as busy forever.
const SESSION_STALE: chrono::Duration = chrono::Duration::seconds(45);

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
}

pub struct Tools {
    godspeed: Arc<Godspeed>,
    // Remember successful sends on this MCP connection so wait_for_turn after
    // wait:false also waits for a newly created chat with no session row yet.
    pending_turns: tokio::sync::Mutex<HashMap<String, Arc<PendingTurn>>>,
}

fn chat_key_schema(extra: Value) -> Value {
    let mut properties = json!({
        "chat": {
            "type": "string",
            "description": "Chat id, unique id prefix, or exact title."
        }
    });
    if let (Some(base), Some(more)) = (properties.as_object_mut(), extra.as_object()) {
        for (k, v) in more {
            base.insert(k.clone(), v.clone());
        }
    }
    json!({ "type": "object", "properties": properties, "required": ["chat"] })
}

fn device_schema() -> Value {
    json!({ "type": "string", "description": "Host device id or exact name. Omit for the local engine." })
}

fn catalog() -> Vec<ToolDef> {
    let mut tools = vec![
        ToolDef {
            name: "whoami",
            description: "Which chat and device this server speaks for, plus the engine's workspace mode. Call this first when you need to know your own chat id.",
            input_schema: json!({ "type": "object", "properties": {} }),
        },
        ToolDef {
            name: "list_devices",
            description: "Devices in this workspace (the local engine's device is flagged). Chats and projects are hosted on a device.",
            input_schema: json!({ "type": "object", "properties": {} }),
        },
        ToolDef {
            name: "list_projects",
            description: "Projects: folders on specific devices. Filter by device (id or name); omit for all projects. Use ids when names or paths repeat.",
            input_schema: json!({ "type": "object", "properties": { "device": device_schema() } }),
        },
        ToolDef {
            name: "list_harnesses",
            description: "Agent harnesses (claude-code, codex, cursor, …) available on the chosen device. Omit device for the local engine.",
            input_schema: json!({ "type": "object", "properties": { "device": device_schema() } }),
        },
        ToolDef {
            name: "list_models",
            description: "Models a harness offers on the chosen device (local engine when omitted). Model ids are harness-specific strings; pass one to create_chat.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "harness": { "type": "string", "description": "Harness id, e.g. claude-code or codex." },
                    "device": device_schema()
                },
                "required": ["harness"]
            }),
        },
        ToolDef {
            name: "list_chats",
            description: "Chats with their project, host device, harness/model, live status, and last activity. Newest activity first.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "project": { "type": "string", "description": "Only chats in this project (id, path, or name)." },
                    "device": { "type": "string", "description": "Only chats hosted on this device (id or name)." },
                    "include_archived": { "type": "boolean", "default": false },
                    "parent": { "type": "string", "description": "Only chats created by this chat (id, prefix, or title) — e.g. your own id to list the chats you spawned." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 500, "default": 50 }
                }
            }),
        },
        ToolDef {
            name: "get_chat",
            description: "One chat's metadata, live status, and any question its agent is currently blocked on.",
            input_schema: chat_key_schema(json!({})),
        },
        ToolDef {
            name: "create_chat",
            description: "Create a standalone session visible in Sessions with kind chat, or a child with kind side. Omitted kind preserves the default: side with an origin/parent, standalone otherwise. A side chat cannot create chats. Select a project and/or device; catalogs are validated on that host. Optionally send a first prompt and wait for the reply. Returns the new chat id. For parallel delegation use create_chats, or leave wait=false on every launch and wait only after all chats have been started. When several chats will edit the same repository at once, pass worktree \"new\" so each works in its own git worktree and branch instead of overwriting each other's files.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "kind": { "type": "string", "enum": ["chat", "side"], "description": "chat: standalone, no parent allowed. side: requires parent or origin. Omit for legacy defaults." },
                    "project": { "type": "string", "description": "Project id, path, or name. Belongs to a specific device; when device is supplied, search only there. Optional." },
                    "device": { "type": "string", "description": "Host device (id or name). With project, it must own that project. Without either, use the local engine." },
                    "parent": { "type": "string", "description": "Parent chat to record (id, prefix, or title). Defaults to the chat you are speaking from." },
                    "notify": { "type": "boolean", "default": false, "description": "Wake this parent with durable notifications when the child needs approval, completes, or fails. Requires parent and child on the same device." },
                    "harness": { "type": "string", "description": "Harness id (see list_harnesses with the chosen device). Defaults to claude-code when available." },
                    "model": { "type": "string", "description": "Model id from list_models on the chosen device. Omit for the harness default." },
                    "reasoning": { "type": "string", "description": "Reasoning level the model supports (e.g. low, medium, high, max)." },
                    "sandbox": { "type": "string", "enum": ["read-only", "workspace-write", "danger-full-access"], "default": "workspace-write" },
                    "access_mode": { "type": "string", "enum": ["supervised", "auto-accept-edits", "auto", "full-access"], "description": "What the new chat's agent may do before it asks. Defaults to your own chat's mode and never exceeds it." },
                    "title": { "type": "string", "description": "Sidebar title. Otherwise the engine titles it from the first exchange." },
                    "branch": { "type": "string", "description": "Branch label to record on the chat." },
                    "cwd": { "type": "string", "description": "Working directory override (any folder on the host). Defaults to the parent's checkout or the project folder. Prefer worktree for checkouts." },
                    "worktree": { "type": "string", "description": WORKTREE_ARG_DESCRIPTION },
                    "base": { "type": "string", "description": "With worktree \"new\": the branch, tag or commit the new worktree starts from. Defaults to the parent chat's branch, else the project's checked-out branch." },
                    "prompt": { "type": "string", "description": "First message to send right away." },
                    "wait": { "type": "boolean", "default": false, "description": "With prompt: block until the first turn finishes and return the reply." },
                    "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 3600, "default": 600 }
                }
            }),
        },
        ToolDef {
            name: "read_chat",
            description: "Read a chat's transcript as plain messages (newest window by default). Tool calls are summarized one per line.",
            input_schema: chat_key_schema(json!({
                "limit": { "type": "integer", "minimum": 1, "maximum": 500, "default": 40, "description": "How many messages, counted from the newest." },
                "offset": { "type": "integer", "minimum": 0, "default": 0, "description": "Skip this many newest messages (paging backwards)." },
                "include_reasoning": { "type": "boolean", "default": false },
                "include_tools": { "type": "boolean", "default": true }
            })),
        },
        ToolDef {
            name: "send_message",
            description: "Send a message to a chat. Messages are attributed to your chat. mode 'auto' starts a turn when the chat is idle, steers a running turn through its live mailbox (at the next supported input boundary without interrupting the agent). Use mode 'queue' only to explicitly hold a message for later. With wait=true, blocks until the turn finishes and returns the assistant's reply. For parallel work use send_messages, or send to all chats with wait=false before waiting.",
            input_schema: chat_key_schema(json!({
                "text": { "type": "string" },
                "mode": { "type": "string", "enum": ["auto", "run", "steer", "queue"], "default": "auto" },
                "wait": { "type": "boolean", "default": false },
                "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 3600, "default": 600 }
            })),
        },
        ToolDef {
            name: "wait_for_turn",
            description: "Block until a chat is no longer working: returns completed, awaitingInput (answer with respond_to_input), errored, or timedOut, with the newest assistant message.",
            input_schema: chat_key_schema(json!({
                "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 3600, "default": 600 }
            })),
        },
        ToolDef {
            name: "interrupt_chat",
            description: "Stop the chat's running turn.",
            input_schema: chat_key_schema(json!({})),
        },
        ToolDef {
            name: "respond_to_input",
            description: "Answer a question the chat's agent is blocked on (see get_chat / wait_for_turn pendingInput). Labels are the option strings, or free text for open questions. A question with an `approval` is a permission request: answer with one of its decision labels (or approve / approveForSession / decline). You may approve only what your own chat's access mode would let you do without asking (full access: anything; auto-accept edits and auto: file edits; supervised: nothing); declining is always allowed.",
            input_schema: chat_key_schema(json!({
                "request_id": { "type": "string", "description": "The pending request id. Defaults to the chat's current pending question." },
                "answers": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "question_id": { "type": "string" },
                            "labels": { "type": "array", "items": { "type": "string" } }
                        },
                        "required": ["question_id", "labels"]
                    }
                }
            })),
        },
        ToolDef {
            name: "archive_chat",
            description: "Archive a chat (hide it from the active sidebar list); pass archived=false to restore. Use it to tidy up chats you created.",
            input_schema: chat_key_schema(json!({
                "archived": { "type": "boolean", "default": true }
            })),
        },
        ToolDef {
            name: CONDUCTOR_INFO,
            description: "The conductor you belong to: its folder, the repos it covers (project id, path, name) and every sub-agent in its tree (chat id, title, depth, parent, status). Call it before starting a sub-agent so you can reuse one that already owns the work.",
            input_schema: json!({ "type": "object", "properties": {} }),
        },
        ToolDef {
            name: "conductor_notes_read",
            description: "Read your conductor's project notes: how the user works, conventions and decisions worth keeping.",
            input_schema: json!({ "type": "object", "properties": {} }),
        },
        ToolDef {
            name: "conductor_notes_write",
            description: "Replace your conductor's project notes with `notes` (up to 12 KB; read them first and keep what still holds). They lead every fresh session of the conductor's agents.",
            input_schema: json!({
                "type": "object",
                "properties": { "notes": { "type": "string" } },
                "required": ["notes"]
            }),
        },
    ];
    for (name, single, description) in [
        (
            "create_chats",
            "create_chat",
            "Create multiple standalone or side chats concurrently (kind per request). Put each chat's prompt in its request to start all work together. Prefer this for parallel delegation, including harnesses that execute tool calls sequentially. Each request has create_chat arguments; wait defaults to false. Give every request that edits the same repository worktree \"new\" so the chats don't edit one checkout at once. Results preserve request order and include per-request errors; successful requests are not rolled back.",
        ),
        (
            "send_messages",
            "send_message",
            "Send messages to multiple independent chats concurrently. Prefer this to delegate parallel work to existing chats. Each request has send_message arguments; wait defaults to false. If wait=true, requests still run concurrently. Results preserve request order and include per-request errors; successful sends are not rolled back. Do not include dependent messages to the same chat.",
        ),
    ] {
        let item_schema = tools
            .iter()
            .find(|tool| tool.name == single)
            .unwrap()
            .input_schema
            .clone();
        tools.push(ToolDef {
            name, description,
            input_schema: json!({
                "type": "object",
                "properties": {"requests": {"type": "array", "minItems": 1, "maxItems": MAX_BATCH, "items": item_schema}},
                "required": ["requests"],
            }),
        });
    }
    tools.push(ToolDef {
        name: SHOW_VISUAL,
        description: SHOW_VISUAL_DESCRIPTION,
        input_schema: json!({
            "type": "object",
            "properties": {
                "script": {
                    "type": "string",
                    "description": "The Luau visual script: setup at the top level, ending with `return function() return ui.column { ... } end`."
                },
                "title": { "type": "string", "description": "Short name for the visual." },
                "assets": {
                    "type": "object",
                    "additionalProperties": { "type": "string" },
                    "description": "Files the script uses, by the name it uses them: { \"prism.glb\": \"models/prism.glb\", \"sky.hdr\": \"/tmp/sky.hdr\" }. Paths are on this machine, relative to your working directory. glTF models (.glb, or .gltf with everything embedded), PNG, JPEG, WebP, and Radiance .hdr images; at most 16 files, 8 MB each, 10 MB together."
                }
            },
            "required": ["script", "title"]
        }),
    });
    tools.push(ToolDef {
        name: SHOW_IMAGE,
        description: SHOW_IMAGE_DESCRIPTION,
        input_schema: json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "The image file on this machine, absolute or relative to your working directory."
                }
            },
            "required": ["path"]
        }),
    });
    tools.extend(crate::preview::catalog());
    tools
}

const WORKTREE_ARG_DESCRIPTION: &str = "\
Where the chat works, in its project's repository. \"parent\" (default): share the parent chat's \
checkout (the project folder without a parent). \"new\": a fresh git worktree on its own new branch, \
started from the parent's branch (or base) and set up like worktrees made in the app (copied .env \
files, reserved ports, setup commands); use it for parallel edits so chats don't overwrite each \
other. Or an existing worktree of the project, by folder path or by the branch it has checked out. \
Needs a project; can't be combined with cwd.";

/// Shows a page in the CALLING chat, so it is offered only to an agent that
/// Godspeed launched (`GODSPEED_CHAT_ID` set).
const SHOW_VISUAL: &str = "show_visual";

/// Conductor tools are for agents inside a Godspeed chat, like `show_visual`.
const CONDUCTOR_INFO: &str = "conductor_info";
fn is_conductor_tool(name: &str) -> bool {
    matches!(
        name,
        CONDUCTOR_INFO | "conductor_notes_read" | "conductor_notes_write"
    )
}
/// Shows an image file in the CALLING chat; offered like `show_visual`.
const SHOW_IMAGE: &str = "show_image";

const SHOW_IMAGE_DESCRIPTION: &str = "\
Show an image file (a screenshot, a rendered chart or diagram, a photo) inline in this chat, above \
your final text reply; call it before writing that reply, once per image, in the order they should \
appear. PNG, JPEG, WebP, or GIF, at most 10 MB and 4096 pixels on a side. Godspeed copies the file \
into the chat, so it stays visible after the file changes or is deleted. The reader \
already sees the image, so the reply should not announce or describe it: add only what the image \
doesn't show.";

const SHOW_VISUAL_DESCRIPTION: &str = "\
Show an interactive visual (chart, table, dashboard, diagram, simulation, 3D view) inline in this \
chat, above your final text reply; call it before writing that reply. Use it when a visual says more \
than prose. The reader already sees it, so the reply should not announce, locate, or restate it: add \
only what the visual doesn't say. To show an image file as it is (a screenshot, a photo), use \
show_image instead.\n\
\n\
A visual is a Luau script (Lua 5.1 plus `+=`, `continue`, and `if` expressions). Godspeed runs it in a \
sandbox (no I/O or network; math.random is seeded, so data is the same on every render) and renders \
what it returns with Godspeed's own native interface. Build data and state at the top level, then return \
a view function. Godspeed calls the view again after every handler, so handlers just assign to state:\n\
\n\
local s = ui.state { metric = \"p95\" }\n\
local data = { { region = \"us-east\", p95 = 182, p99 = 240 }, { region = \"eu-west\", p95 = 140, p99 = 210 } }\n\
return function()\n\
  return ui.column { gap = 16,\n\
    ui.text { \"Latency by region\", size = \"lg\", weight = 600 },\n\
    ui.tabs { options = { \"p95\", \"p99\" }, value = s.metric, on_change = function(v) s.metric = v end },\n\
    ui.chart { type = \"bar\", height = 220, unit = \"ms\",\n\
      x = ui.map(data, function(d) return d.region end),\n\
      series = { { name = s.metric, values = ui.map(data, function(d) return d[s.metric] end) } } },\n\
  }\n\
end\n\
\n\
Godspeed runs the script before showing it (setup, render, every draw, and each tab, toggle, slider, and \
button once) and returns the first error with its line; fix it and call again.\n\
\n\
Nodes are props tables: children are the array part, strings are plain text, nil and false children \
are skipped, and lists of nodes flatten (so ui.map results can be children). Unknown props are errors.\n\
- ui.column { gap = 8, align = \"start|center|end\", ... }, ui.row { gap = 12, align, justify = \
\"start|center|end|between\", wrap = true, ... }, ui.grid { columns = 3, gap = 16, ... }. Any node \
takes width = px or grow = true inside a row (sliders, charts, tables, canvases, and scenes fill a \
row's spare width by default), and key = \"name\" to keep its hover or camera when the tree changes \
shape. A row or column with on_click = function() end is clickable; hover = true gives \
it the hover wash.\n\
- ui.text { \"plain \", ui.span { \"bold\", weight = 600, color = \"accent\", mono = true }, size = \
\"xs|sm|body|lg|xl|2xl\", weight = 400-700, color, mono, italic, align, truncate }, or ui.text \"string\".\n\
- ui.stat { value = \"182 ms\", label = \"p95 latency\", delta = \"-12%\", tone = \"success\" }: a headline \
figure over a muted label; tone colors the delta.\n\
- ui.table { columns = { \"Region\", { label = \"p95\", align = \"end\" } }, rows = { { \"us-east\", 182 }, \
... } }: laid out like a markdown table (bold header, hairline row rules), numbers right-aligned; cells \
are strings, numbers, or nodes.\n\
- ui.chart { type = \"bar|line|area|scatter\", height = 220, x = { labels }, series = { { name, values = \
{ ... }, color } }, stacked, horizontal (bar), min, max, unit = \"ms\", prefix = \"$\", decimals, legend }. \
Scatter series take points = { { x, y }, ... } (x_unit, x_decimals). Axes, gridlines, the legend, and a \
hover readout are built in.\n\
- ui.rule {} is a hairline divider; ui.space { size = 16 }; ui.link { label, url = \"https://...\" } \
opens like a chat link.\n\
- Controls: ui.tabs { options = { ... }, value, on_change = function(option) end }, ui.toggle { label, \
value = bool, on_change = function(on) end }, ui.slider { label, value, min, max, step, unit, on_change \
= function(n) end }, ui.button { label, on_click = function() end, primary = bool }.\n\
- ui.canvas { height = 240, draw = function(g, w, h, t) ... end, animate = bool, on_click = \
function(x, y) end }: 2D drawing in pixels, origin top-left, w = the reply column's width. \
g:line(x1, y1, x2, y2, { color, width, dash }), g:polyline(pts, { color, width }), g:polygon(pts, \
{ fill, stroke, width }), g:rect(x, y, w, h, { fill, stroke, radius }), g:circle(x, y, r, { fill, \
stroke }), g:arc(x, y, r, a0, a1, { color } or { fill, inner = radius } for pie and donut sectors; \
radians clockwise from 3 o'clock), g:text(x, y, str, { size, color, weight, align = \
\"start|center|end\", baseline = \"top|middle|bottom\", mono }). pts are { x1, y1, x2, y2, ... } or \
{ { x, y }, ... }. g.pointer is { x, y, down } while the pointer is over the canvas (draw hover \
effects with it); t is seconds, and animate = true redraws every frame while visible.\n\
- ui.scene { height = 320, draw = function(s, t) ... end, orbit = true, spin = 0.3 (turntable \
radians per second), camera = { yaw = 0.6, pitch = 0.45, zoom = 1 }, animate = bool }: real 3D, y up, \
rendered on the GPU with depth, soft shadows from a key light, and bloom. Godspeed frames the camera on \
your geometry and the reader drags to orbit. Solids (box, sphere, cylinder, surface, mesh) take \
material props: roughness (0 mirror to 1 matte), metal (0-1), glow (emits its color and blooms; \
use for lamps, beams, highlights), glass = true (refracts what's behind it; ior = 1.5, dispersion \
= 0-1 splits it into a rainbow, as a prism does). Build real forms from several primitives (legs, \
tops, rims), not single boxes, and give materials so surfaces read as wood, metal, or glass. s:box { at = { x, y, z }, size = { w, h, d } or n, color }, s:sphere { at, radius, color }, \
s:cylinder { at (base center), radius, height, color }, s:surface { x = { min, max }, z = { min, max }, \
f = function(x, z) return y end or values = { { ... } }, detail = 32, colors = { \"chart1\", \"chart3\" } \
(a ramp by height), wire = bool }, s:mesh { vertices = { { x, y, z }, ... }, faces = { { 1, 2, 3 }, ... }, \
color }, s:line(a, b, { color, width }), s:path(points, { color, width }), s:point(at, { radius, \
color, hint = \"shown on hover\" }), s:label(at, text, { color, size }), s:grid { size = 10, step = 1 }, \
s:axes { size = 1, labels = { \"x\", \"y\", \"z\" } }. Physical colors may be \"#rrggbb\" in scenes \
(a material's own color: gold, glass, wood), and only there.\n\
- Assets: pass real files in show_visual's assets (name -> path) and scripts use them by name. \
s:model { asset = \"prism.glb\", at, rotate = { x, y, z } degrees, scale, fit = 2 (center it and size \
its longest side), color and material props override the model's own } draws a glTF model with its \
materials and textures. Solids take texture = \"wood.jpg\" (multiplied by color; mesh takes uvs = { { u, v \
}, ... } per vertex). s:environment { asset = \"studio.hdr\", intensity = 1, background = false } lights \
the scene from an equirectangular image (an HDR is best) that metal and glass reflect and glass bends; \
background = true also draws it behind the scene. s:sprite { at, size = 48 (pixels), color, glow = 1, \
asset = \"flare.png\" } adds a camera-facing light (a flare or glint; a soft round glow without an \
asset) that blooms. ui.shader takes textures = { \"rock.jpg\", ... } as iTexture0-iTexture3 \
(repeating, mipmapped). Find or make assets with your own tools (download, convert KTX2 or EXR to PNG \
or HDR) before calling.\n\
- ui.shader { height = 360, code = [[ GLSL ]], buffers = { [[ GLSL ]], ... }, inputs = { speed = \
s.speed }, blend = \"alpha|light|opaque\", bloom = 0-3, orbit = bool, spin, camera = { yaw, pitch, \
zoom }, animate = true }: GPU shaders for what geometry can't express: raymarched SDFs and \
fractals, light and optics (beams, refraction, dispersion), volumes, fluids, particles, generative \
art. Write each pass as on Shadertoy: void mainImage(out vec4 fragColor, in vec2 fragCoord), with \
iResolution (vec3: width and height in pixels, then the display's pixel density), iTime, \
iTimeDelta, iFrame, iMouse (pixels from the bottom-left; xy where the pointer is, zw where it went \
down while it's held, negative otherwise), iCamera (vec4 yaw, pitch, zoom, 0; orbit = true lets the reader drag \
it), every inputs name as a float (bind them to state so sliders and toggles drive the shader), \
iNoise (a tiling 256x256 random RGBA texture), and iChannel0-iChannel3 with iChannelResolution: \
buffers run first, in order, each into a float texture that every pass reads as iChannel<i> (an \
earlier buffer's current frame; a buffer's own previous frame), for multi-pass work like blur, \
trails, accumulation, and simulation. The shader sits on the chat, never in a box: don't paint a \
background, vignette, or floor color. By default (blend = \"light\") the output is emitted light: \
brightness is opacity and black is clear, right for beams, glows, sparks, and light through glass. \
For solid shaded objects (fractals, SDF bodies) set blend = \"alpha\" and write alpha 0 where the ray \
misses. bloom = 0.6-1.5 adds a soft glow around bright light. \
The width varies with the reader (a phone is about 350px wide, often narrower than the shader is \
tall): scale the scene by min(iResolution.x, iResolution.y), not one axis, and keep the subject \
centered with room on both sides, so it fits and stays centered at any aspect. \
GLSL errors come back with the pass and line number.\n\
- Helpers: ui.scale(d0, d1, r0, r1) returns a mapping function; ui.ticks(lo, hi, n); ui.fmt(n, { unit, \
prefix, decimals }) groups digits; ui.map(list, function(v, i) end); ui.range(a, b, step); ui.clamp; \
ui.lerp; ui.series(i) is \"chart<i>\"; ui.color(token, alpha); ui.mix(a, b, t). print(...) output comes \
back to you.\n\
\n\
Colors are theme tokens, never hex or color names (except scene materials): text; muted (labels, axes, captions); faint \
(tertiary notes); accent (the one thing to notice); on-accent (text on an accent mark); success, \
warning, danger (status only); border (hairlines, gridlines); chart1, chart2, chart3, chart4, \
chart5, chart6 (series, in order; \
chart1 is the accent). They follow the reader's theme, light or dark, on frosted glass or an opaque \
surface, so never assume a background color.\n\
\n\
Style, enforced: layout nodes have no fill, border, shadow, padding, or font props. A visual is part \
of the reply, not a widget, so lay content directly on the chat with type, spacing, and alignment: no \
cards, tiles, boxed panels, banners, or canvas backgrounds. Stats are a row of plain ui.stat figures; \
a chart is a plain heading, then the chart; separate sections with gap or one ui.rule. Text renders \
in Godspeed's interface font at the reader's text size: body for most text, sm and xs for labels and \
notes, lg and xl for headings, 2xl for a headline figure. The width is the reply column (about 720px, \
as narrow as 360px), so lay out fluidly with grow, wrap, and grid, and give charts, canvases, and \
scenes heights of 160-420px.";

/// Read `show_visual`'s asset files (paths relative to the agent's working
/// directory), base64-encoded by name for the engine.
async fn read_visual_assets(
    assets: &std::collections::BTreeMap<String, String>,
) -> anyhow::Result<serde_json::Map<String, Value>> {
    use base64::Engine as _;
    use zeron_proto::{
        VISUAL_MAX_ASSET_BYTES as MAX_ASSET_BYTES, VISUAL_MAX_ASSETS as MAX_ASSETS,
        VISUAL_MAX_TOTAL_ASSET_BYTES as MAX_TOTAL_ASSET_BYTES,
        valid_visual_asset_name as valid_name,
    };
    anyhow::ensure!(
        assets.len() <= MAX_ASSETS,
        "a visual takes at most {MAX_ASSETS} assets"
    );
    let mut out = serde_json::Map::new();
    let mut total = 0;
    let mut seen = std::collections::HashSet::new();
    for (name, path) in assets {
        anyhow::ensure!(
            valid_name(name),
            "asset name '{name}' must be a plain file name (letters, digits, '.', '-', '_')"
        );
        // Stored as files, and file systems often ignore case.
        anyhow::ensure!(
            seen.insert(name.to_ascii_lowercase()),
            "asset names must differ by more than case ('{name}')"
        );
        let metadata = tokio::fs::metadata(path)
            .await
            .with_context(|| format!("asset '{name}': can't read {path}"))?;
        anyhow::ensure!(metadata.is_file(), "asset '{name}': {path} is not a file");
        anyhow::ensure!(
            metadata.len() <= MAX_ASSET_BYTES as u64,
            "asset '{name}' is over {} MB",
            MAX_ASSET_BYTES / (1024 * 1024)
        );
        // Bounded however the file changes (or lies about its size).
        let mut bytes = Vec::new();
        {
            use tokio::io::AsyncReadExt as _;
            tokio::fs::File::open(path)
                .await
                .with_context(|| format!("asset '{name}': can't read {path}"))?
                .take(MAX_ASSET_BYTES as u64 + 1)
                .read_to_end(&mut bytes)
                .await
                .with_context(|| format!("asset '{name}': can't read {path}"))?;
        }
        anyhow::ensure!(
            bytes.len() <= MAX_ASSET_BYTES,
            "asset '{name}' is over {} MB",
            MAX_ASSET_BYTES / (1024 * 1024)
        );
        total += bytes.len();
        anyhow::ensure!(
            total <= MAX_TOTAL_ASSET_BYTES,
            "a visual's assets total at most {} MB",
            MAX_TOTAL_ASSET_BYTES / (1024 * 1024)
        );
        out.insert(
            name.clone(),
            Value::String(base64::engine::general_purpose::STANDARD.encode(bytes)),
        );
    }
    Ok(out)
}

/// Read `show_image`'s file (a path relative to the agent's working
/// directory), bounded however the file changes (or lies about its size).
async fn read_image(path: &str) -> anyhow::Result<Vec<u8>> {
    use tokio::io::AsyncReadExt as _;
    use zeron_proto::IMAGE_MAX_BYTES;
    let too_large = || anyhow::anyhow!("{path} is over {} MB", IMAGE_MAX_BYTES / (1024 * 1024));
    let metadata = tokio::fs::metadata(path)
        .await
        .with_context(|| format!("can't read {path}"))?;
    anyhow::ensure!(metadata.is_file(), "{path} is not a file");
    if metadata.len() > IMAGE_MAX_BYTES as u64 {
        return Err(too_large());
    }
    let mut bytes = Vec::new();
    tokio::fs::File::open(path)
        .await
        .with_context(|| format!("can't read {path}"))?
        .take(IMAGE_MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .await
        .with_context(|| format!("can't read {path}"))?;
    if bytes.len() > IMAGE_MAX_BYTES {
        return Err(too_large());
    }
    Ok(bytes)
}

// ---- argument shapes ---------------------------------------------------------

#[derive(Deserialize)]
struct ShowImageArgs {
    path: String,
}

#[derive(Deserialize)]
struct ShowVisualArgs {
    script: String,
    title: String,
    #[serde(default)]
    assets: std::collections::BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct BatchArgs {
    requests: Vec<Value>,
}

#[derive(Deserialize)]
struct ChatArgs {
    chat: String,
}

#[derive(Deserialize)]
struct ListModelsArgs {
    harness: String,
    device: Option<String>,
}

#[derive(Deserialize, Default)]
struct ListChatsArgs {
    project: Option<String>,
    device: Option<String>,
    #[serde(default)]
    include_archived: bool,
    parent: Option<String>,
    limit: Option<usize>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum ChatKind {
    Chat,
    Side,
}

#[derive(Deserialize, Default)]
struct DeviceArgs {
    device: Option<String>,
}

/// Messages already in the transcript before a send. Replies are matched by
/// id, not by time: `createdAt` is stamped on the host's clock, which can run
/// behind the caller's when the chat lives on another device.
#[derive(Clone, Default)]
struct TurnStart {
    message_ids: Vec<String>,
}

#[derive(Clone)]
struct PendingTurn {
    baseline: Option<Session>,
    start: TurnStart,
}

#[derive(Deserialize, Default)]
struct CreateChatArgs {
    kind: Option<ChatKind>,
    project: Option<String>,
    device: Option<String>,
    parent: Option<String>,
    #[serde(default)]
    notify: bool,
    harness: Option<String>,
    model: Option<String>,
    reasoning: Option<String>,
    sandbox: Option<String>,
    access_mode: Option<String>,
    title: Option<String>,
    branch: Option<String>,
    cwd: Option<String>,
    worktree: Option<String>,
    base: Option<String>,
    prompt: Option<String>,
    #[serde(default)]
    wait: bool,
    timeout_secs: Option<u64>,
}

/// `create_chat`'s `worktree` argument.
#[derive(Debug, Clone, PartialEq, Eq)]
enum WorktreeChoice {
    /// The parent's checkout (today's behaviour).
    Parent,
    /// A fresh worktree on its own branch.
    New,
    /// An existing checkout, by folder or branch.
    Existing(String),
}

impl WorktreeChoice {
    fn parse(raw: Option<&str>) -> Self {
        match raw.map(str::trim).filter(|w| !w.is_empty()) {
            None => Self::Parent,
            Some(w) if w.eq_ignore_ascii_case("parent") => Self::Parent,
            Some(w) if w.eq_ignore_ascii_case("new") => Self::New,
            Some(w) => Self::Existing(w.to_owned()),
        }
    }
}

/// Where `place_chat` put a new chat.
struct Placement {
    /// `parent`, `new` or `existing`, as reported back.
    mode: &'static str,
    path: String,
    branch: Option<String>,
    setup_error: Option<String>,
}

#[derive(Deserialize)]
struct ReadChatArgs {
    chat: String,
    limit: Option<usize>,
    #[serde(default)]
    offset: usize,
    #[serde(default)]
    include_reasoning: bool,
    include_tools: Option<bool>,
}

#[derive(Deserialize)]
struct SendArgs {
    chat: String,
    text: String,
    mode: Option<String>,
    #[serde(default)]
    wait: bool,
    timeout_secs: Option<u64>,
}

#[derive(Deserialize)]
struct WaitArgs {
    chat: String,
    timeout_secs: Option<u64>,
}

#[derive(Deserialize)]
struct NotesArgs {
    notes: String,
}

#[derive(Deserialize)]
struct ArchiveArgs {
    chat: String,
    archived: Option<bool>,
}

#[derive(Deserialize)]
struct RespondArgs {
    chat: String,
    request_id: Option<String>,
    #[serde(default)]
    answers: Vec<AnswerArg>,
}

#[derive(Deserialize)]
struct AnswerArg {
    question_id: String,
    labels: Vec<String>,
}

fn parse<T: serde::de::DeserializeOwned>(args: Value) -> Result<T, String> {
    serde_json::from_value(args).map_err(|e| format!("invalid arguments: {e}"))
}

fn wait_duration(secs: Option<u64>) -> Duration {
    secs.map(Duration::from_secs)
        .unwrap_or(DEFAULT_WAIT)
        .min(MAX_WAIT)
}

#[cfg(test)]
fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

fn parse_enum<T: serde::de::DeserializeOwned>(what: &str, raw: &str) -> Result<T, String> {
    serde_json::from_value(Value::String(raw.trim().to_owned()))
        .map_err(|_| format!("unknown {what}: {raw:?}"))
}

// ---- summaries ---------------------------------------------------------------

/// Live posture of one chat as the tools report it.
fn status_of(session: Option<&Session>) -> (String, Option<i64>) {
    let Some(session) = session else {
        return ("idle".into(), None);
    };
    let age = chrono::Utc::now() - session.updated_at;
    let stale = age > SESSION_STALE;
    let label = match session.status {
        SessionStatus::Working if stale => "working (stale — host may be offline)",
        SessionStatus::Working => "working",
        SessionStatus::AwaitingInput => "awaitingInput",
        SessionStatus::Errored => "errored",
        SessionStatus::Idle => "idle",
    };
    (label.into(), Some(age.num_seconds().max(0)))
}

fn summarize_chat(chat: &Chat, spaces: &[Space], sessions: &[Session]) -> Value {
    let space = chat
        .space_id
        .as_deref()
        .and_then(|id| spaces.iter().find(|s| s.id == id));
    let session = session_for(sessions, chat);
    let (status, status_age) = status_of(session.as_ref());
    json!({
        "id": chat.id,
        "title": chat.title,
        "project": space.map(|s| json!({
            "id": s.id,
            "name": s.display_name(),
            "path": s.path,
        })),
        "deviceId": chat.device_id,
        "cwd": chat.cwd.clone().or_else(|| space.map(|s| s.path.clone())),
        "branch": chat.branch,
        "harness": chat.config.as_ref().map(|c| c.harness),
        "model": chat.config.as_ref().and_then(|c| c.model.clone()),
        "reasoning": chat.config.as_ref().and_then(|c| c.reasoning),
        "archived": chat.archived,
        "parentChatId": chat.parent_chat_id,
        "status": status,
        "statusAgeSecs": status_age,
        "lastMessageAt": chat.last_message_at,
        "lastMessagePreview": chat.last_message_preview,
        "createdAt": chat.created_at,
    })
}

fn last_pending_input(messages: &[RenderedMessage]) -> Option<Value> {
    messages.iter().rev().find_map(|m| m.pending_input.clone())
}

// ---- dispatch ----------------------------------------------------------------

impl Tools {
    pub fn new(godspeed: Arc<Godspeed>) -> Self {
        Self {
            godspeed,
            pending_turns: Default::default(),
        }
    }

    /// The tools offered to this server's chat. Conductor tools are listed
    /// only for conductor members.
    pub async fn list(&self) -> Vec<ToolDef> {
        let member = self
            .origin_conductor_membership()
            .await
            .is_ok_and(|m| m.is_some());
        self.callable()
            .filter(|tool| member || !is_conductor_tool(tool.name))
            .collect()
    }

    /// Whether `name` may be called here. Conductor tools are callable from
    /// any chat and refuse outside a conductor themselves, so this needs no
    /// engine round trip per call.
    pub fn has(&self, name: &str) -> bool {
        self.callable().any(|t| t.name == name)
    }

    fn callable(&self) -> impl Iterator<Item = ToolDef> {
        let in_chat = self.godspeed.origin().chat_id.is_some();
        catalog().into_iter().filter(move |tool| {
            in_chat
                || (![SHOW_VISUAL, SHOW_IMAGE].contains(&tool.name)
                    && !is_conductor_tool(tool.name)
                    && !crate::preview::is_preview_tool(tool.name))
        })
    }

    /// `Ok` is the tool's structured result; `Err` is a message the model
    /// should read (surfaced as `isError`, never as a protocol error).
    pub async fn call(&self, name: &str, args: Value) -> Result<Value, String> {
        let result = match name {
            "whoami" => self.whoami().await,
            "list_devices" => self.list_devices().await,
            "list_projects" => self.list_projects(parse(args)?).await,
            "list_harnesses" => self.list_harnesses(parse(args)?).await,
            "list_models" => self.list_models(parse(args)?).await,
            "list_chats" => self.list_chats(parse(args)?).await,
            "get_chat" => self.get_chat(parse(args)?).await,
            "create_chat" => self.create_chat(parse(args)?).await,
            "create_chats" => self.batch(parse(args)?, true).await,
            "send_messages" => self.batch(parse(args)?, false).await,
            "read_chat" => self.read_chat(parse(args)?).await,
            "send_message" => self.send_message(parse(args)?).await,
            "wait_for_turn" => self.wait_for_turn(parse(args)?).await,
            "interrupt_chat" => self.interrupt_chat(parse(args)?).await,
            "respond_to_input" => self.respond_to_input(parse(args)?).await,
            "archive_chat" => self.archive_chat(parse(args)?).await,
            SHOW_VISUAL => self.show_visual(parse(args)?).await,
            CONDUCTOR_INFO => self.conductor_info().await,
            "conductor_notes_read" => self.conductor_notes_read().await,
            "conductor_notes_write" => self.conductor_notes_write(parse(args)?).await,
            SHOW_IMAGE => self.show_image(parse(args)?).await,
            name if crate::preview::is_preview_tool(name) => return self.preview(name, args).await,
            other => return Err(format!("unknown tool: {other}")),
        };
        result.map_err(|e| format!("{e:#}"))
    }

    /// Poll every request together, including its optional wait. A waiting
    /// first chat must not prevent subsequent chats from receiving their work.
    async fn batch(&self, args: BatchArgs, create: bool) -> anyhow::Result<Value> {
        anyhow::ensure!(
            (1..=MAX_BATCH).contains(&args.requests.len()),
            "requests must contain between 1 and {MAX_BATCH} items"
        );
        let results = futures::future::join_all(args.requests.into_iter().enumerate().map(
            |(index, args)| async move {
                let result = if create {
                    match serde_json::from_value(args) {
                        Ok(args) => self.create_chat(args).await,
                        Err(error) => Err(error.into()),
                    }
                } else {
                    match serde_json::from_value(args) {
                        Ok(args) => self.send_message(args).await,
                        Err(error) => Err(error.into()),
                    }
                };
                match result {
                    Ok(result) => json!({"index": index, "isError": false, "result": result}),
                    Err(error) => {
                        json!({"index": index, "isError": true, "error": error.to_string()})
                    }
                }
            },
        ))
        .await;
        Ok(json!({"results": results}))
    }

    async fn whoami(&self) -> anyhow::Result<Value> {
        let origin = self.godspeed.origin().clone();
        let local_device = self.godspeed.local_device_id().await?;
        let engine = self.godspeed.engine_info().await.unwrap_or(Value::Null);
        let chat = match origin.chat_id.as_deref() {
            Some(id) => match self.godspeed.resolve_chat(id).await {
                Ok(chat) => {
                    let (spaces, sessions) =
                        tokio::try_join!(self.godspeed.spaces(), self.godspeed.sessions())?;
                    summarize_chat(&chat, &spaces, &sessions)
                }
                Err(_) => json!({ "id": id }),
            },
            None => Value::Null,
        };
        Ok(json!({
            "chat": chat,
            "originDeviceId": origin.device_id,
            "localDeviceId": local_device,
            "workspaceScope": engine.get("workspaceScope").cloned().unwrap_or(Value::Null),
            "note": if origin.chat_id.is_some() {
                "Messages you send are attributed to this chat; it cannot message itself."
            } else {
                "Not running inside a chat: messages are sent without attribution."
            },
        }))
    }

    async fn list_devices(&self) -> anyhow::Result<Value> {
        let (devices, local) =
            tokio::try_join!(self.godspeed.devices(), self.godspeed.local_device_id())?;
        Ok(json!({
            "devices": devices.iter().map(|d| json!({
                "id": d.id,
                "name": d.name,
                "platform": d.platform,
                "local": d.id == local,
                "lastSeenAt": d.last_seen_at,
                "version": d.version,
            })).collect::<Vec<_>>()
        }))
    }

    async fn list_projects(&self, args: DeviceArgs) -> anyhow::Result<Value> {
        let (mut spaces, devices) =
            tokio::try_join!(self.godspeed.spaces(), self.godspeed.devices())?;
        if let Some(device) = args.device.as_deref() {
            let device = self.godspeed.resolve_device_id(Some(device)).await?;
            spaces.retain(|s| s.device_id == device);
        }
        let device_name = |id: &str| devices.iter().find(|d| d.id == id).map(|d| d.name.clone());
        Ok(json!({
            "projects": spaces.iter().map(|s| json!({
                "id": s.id,
                "name": s.display_name(),
                "path": s.path,
                "deviceId": s.device_id,
                "deviceName": device_name(&s.device_id),
                "git": s.git_detected,
            })).collect::<Vec<_>>()
        }))
    }

    async fn list_harnesses(&self, args: DeviceArgs) -> anyhow::Result<Value> {
        let device = match args.device.as_deref() {
            Some(key) => Some(self.godspeed.resolve_device_id(Some(key)).await?),
            None => None,
        };
        let harnesses = self.godspeed.harnesses_on(device.as_deref()).await?;
        Ok(json!({
            "harnesses": harnesses.iter().map(|h| json!({
                "id": h.id,
                "name": h.name,
                "available": h.available(),
                "installed": h.installed,
                "steersMidTurn": h.steers_mid_turn(),
                "reasoningLevels": h.reasoning_levels,
            })).collect::<Vec<_>>()
        }))
    }

    async fn list_models(&self, args: ListModelsArgs) -> anyhow::Result<Value> {
        let harness: HarnessId =
            parse_enum("harness", &args.harness).map_err(anyhow::Error::msg)?;
        let device = match args.device.as_deref() {
            Some(key) => Some(self.godspeed.resolve_device_id(Some(key)).await?),
            None => None,
        };
        let models = self.godspeed.models_on(harness, device.as_deref()).await?;
        Ok(json!({
            "harness": harness,
            "models": models.iter().map(|m| json!({
                "id": m.id,
                "label": m.label,
                "description": m.description,
                "reasoningLevels": m.reasoning_levels,
            })).collect::<Vec<_>>()
        }))
    }

    async fn list_chats(&self, args: ListChatsArgs) -> anyhow::Result<Value> {
        let (mut chats, spaces, sessions) = tokio::try_join!(
            self.godspeed.chats(),
            self.godspeed.spaces(),
            self.godspeed.sessions()
        )?;
        if let Some(project) = args.project.as_deref() {
            let (space, _) = self
                .godspeed
                .resolve_target(Some(project), args.device.as_deref())
                .await?;
            let space = space.expect("project provided");
            chats.retain(|c| c.space_id.as_deref() == Some(space.id.as_str()));
        }
        if args.device.is_some() {
            let device = self
                .godspeed
                .resolve_device_id(args.device.as_deref())
                .await?;
            chats.retain(|c| c.device_id == device);
        }
        if !args.include_archived {
            chats.retain(|c| !c.archived);
        }
        if let Some(parent) = args.parent.as_deref() {
            let parent = self.godspeed.resolve_chat(parent).await?;
            chats.retain(|c| c.parent_chat_id.as_deref() == Some(parent.id.as_str()));
        }
        chats.sort_by(|a, b| {
            let a_at = a.last_message_at.unwrap_or(a.created_at);
            let b_at = b.last_message_at.unwrap_or(b.created_at);
            b_at.cmp(&a_at)
        });
        let total = chats.len();
        let limit = args.limit.unwrap_or(50).clamp(1, 500);
        Ok(json!({
            "total": total,
            "chats": chats.iter().take(limit)
                .map(|c| summarize_chat(c, &spaces, &sessions))
                .collect::<Vec<_>>()
        }))
    }

    async fn get_chat(&self, args: ChatArgs) -> anyhow::Result<Value> {
        let chat = self.godspeed.resolve_chat(&args.chat).await?;
        let (spaces, sessions, entries) = tokio::try_join!(
            self.godspeed.spaces(),
            self.godspeed.sessions(),
            self.godspeed.transcript_on(&chat.id, Some(&chat.device_id))
        )?;
        let rendered = render_entries(&entries, RenderOptions::default());
        let mut summary = summarize_chat(&chat, &spaces, &sessions);
        summary["messageCount"] = json!(rendered.len());
        summary["pendingInput"] = last_pending_input(&rendered).unwrap_or(Value::Null);
        summary["lastMessage"] = rendered.last().map(|m| json!(m)).unwrap_or(Value::Null);
        Ok(summary)
    }

    async fn create_chat(&self, args: CreateChatArgs) -> anyhow::Result<Value> {
        let origin_conductor = match self.godspeed.origin().chat_id.as_deref() {
            Some(origin) => {
                let chat = self.godspeed.resolve_chat(origin).await?;
                self.ensure_may_parent(
                    &chat,
                    "Side chats cannot create chats. Ask your parent chat to create another side chat.",
                )
                .await?;
                self.godspeed.conductor_membership(&chat.id).await?
            }
            None => None,
        };
        // A conductor's agents stay inside its tree, where the depth and
        // fan-out caps and the approval rule hold.
        anyhow::ensure!(
            origin_conductor.is_none() || args.kind != Some(ChatKind::Chat),
            "Conductor agents can't start standalone chats. Omit kind (or use kind side) to start a sub-agent inside this conductor."
        );
        // Parent: the explicit `parent` argument, else the chat this server
        // speaks for. Resolved so a prefix/title works and a typo fails loud.
        let explicit_parent = args
            .parent
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty());
        anyhow::ensure!(
            args.kind != Some(ChatKind::Chat) || explicit_parent.is_none(),
            "kind chat cannot have a parent"
        );
        let parent_chat_id = if args.kind == Some(ChatKind::Chat) {
            None
        } else {
            match explicit_parent {
                Some(key) => Some(self.godspeed.resolve_chat(key).await?.id),
                None => self.godspeed.origin().chat_id.clone(),
            }
            // A hidden voice orchestrator creates top-level sessions.
            .filter(|parent| !zeron_proto::voice::is_orchestrator_chat(parent))
        };
        anyhow::ensure!(
            args.kind != Some(ChatKind::Side) || parent_chat_id.is_some(),
            "kind side requires a parent or origin chat"
        );
        anyhow::ensure!(
            !args.notify || parent_chat_id.is_some(),
            "notify requires a side chat with a parent"
        );
        let kind = if parent_chat_id.is_some() {
            ChatKind::Side
        } else {
            ChatKind::Chat
        };

        if let (Some(member), Some(parent)) = (&origin_conductor, parent_chat_id.as_deref()) {
            let inside = self
                .godspeed
                .conductor_membership(parent)
                .await?
                .is_some_and(|m| m.conductor_id == member.conductor_id);
            anyhow::ensure!(
                inside,
                "Conductor agents start sub-agents inside their own conductor, and {} is outside it.",
                short(parent)
            );
        }
        let parent_chat = match parent_chat_id.as_deref() {
            Some(parent) => {
                let chat = self.godspeed.resolve_chat(parent).await?;
                self.ensure_may_parent(
                    &chat,
                    "Cannot create a child of a side chat. Choose a top-level parent chat.",
                )
                .await?;
                Some(chat)
            }
            None => None,
        };
        let worktree_choice = WorktreeChoice::parse(args.worktree.as_deref());
        let explicit_cwd = args
            .cwd
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .map(str::to_owned);
        anyhow::ensure!(
            worktree_choice == WorktreeChoice::Parent || explicit_cwd.is_none(),
            "worktree and cwd can't be combined: worktree already picks the folder"
        );
        let base = args
            .base
            .as_deref()
            .map(str::trim)
            .filter(|b| !b.is_empty())
            .map(str::to_owned);
        anyhow::ensure!(
            base.is_none() || worktree_choice == WorktreeChoice::New,
            "base only applies to worktree \"new\""
        );
        // A worktree lives in a project: the parent's when none is named.
        let project = args.project.clone().or_else(|| {
            (worktree_choice != WorktreeChoice::Parent)
                .then(|| parent_chat.as_ref()?.space_id.clone())
                .flatten()
        });
        let (space, device_id) = self
            .godspeed
            .resolve_target(project.as_deref(), args.device.as_deref())
            .await?;
        if worktree_choice != WorktreeChoice::Parent {
            let space = space.as_ref().context(
                "worktree needs a project: pass project (a git repository), or create the chat from a parent that has one",
            )?;
            anyhow::ensure!(
                space.git_detected,
                "project {} isn't a git repository, so it has no worktrees",
                space.display_name()
            );
        }
        if args.notify {
            let parent = self
                .godspeed
                .resolve_chat(parent_chat_id.as_deref().expect("validated parent"))
                .await?;
            anyhow::ensure!(
                parent.device_id == device_id,
                "notify is only supported when the parent and child use the same device"
            );
        }
        let harnesses = self.godspeed.harnesses_on(Some(&device_id)).await?;
        let harness = match args.harness.as_deref() {
            Some(raw) => {
                let id: HarnessId = parse_enum("harness", raw).map_err(anyhow::Error::msg)?;
                anyhow::ensure!(
                    harnesses.iter().any(|h| h.id == id && h.available()),
                    "harness {raw} is not available on device {device_id} (see list_harnesses with device)"
                );
                id
            }
            None => default_harness(&harnesses).with_context(|| format!("device {device_id}"))?,
        };
        if let Some(model) = args.model.as_deref() {
            let models = self.godspeed.models_on(harness, Some(&device_id)).await?;
            anyhow::ensure!(
                models.iter().any(|m| m.id == model),
                "model {model:?} is not offered by {harness:?} on device {device_id}; available: {}",
                models
                    .iter()
                    .map(|m| m.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        let reasoning: Option<ReasoningLevel> = match args.reasoning.as_deref() {
            Some(raw) => Some(parse_enum("reasoning level", raw).map_err(anyhow::Error::msg)?),
            None => None,
        };
        let sandbox: SandboxLevel = match args.sandbox.as_deref() {
            Some(raw) => parse_enum("sandbox", raw).map_err(anyhow::Error::msg)?,
            None => SandboxLevel::WorkspaceWrite,
        };
        // A delegated chat never gets more access than the chat delegating
        // to it: it takes the origin's mode unless asked for less.
        let requested: Option<AccessMode> = match (args.access_mode.as_deref(), &args.sandbox) {
            (Some(raw), _) => Some(parse_enum("access mode", raw).map_err(anyhow::Error::msg)?),
            (None, Some(_)) => Some(AccessMode::from_legacy(sandbox, false)),
            (None, None) => None,
        };
        let access_mode = match (requested, self.origin_access_mode().await?) {
            (Some(mode), Some(cap)) => Some(mode.at_most(cap)),
            (mode, cap) => mode.or(cap),
        };
        if let Some(mode) = access_mode {
            anyhow::ensure!(
                zeron_proto::access_mode_supported(harness, mode),
                "{harness:?} can't run in {}; pick another harness or access mode",
                mode.label()
            );
        }
        let config = ChatConfig {
            harness,
            model: args.model.clone(),
            reasoning,
            model_options: Default::default(),
            sandbox: access_mode.map_or(sandbox, |mode| mode.legacy().0),
            access_mode,
        };
        anyhow::ensure!(
            worktree_choice != WorktreeChoice::New || config.sandbox != SandboxLevel::ReadOnly,
            "A read-only chat can't edit files, so it doesn't need its own worktree. Use worktree \"parent\", or a mode that may edit."
        );
        // Every refusal above comes before anything is created on disk.
        let placement = self
            .place_chat(
                &worktree_choice,
                space.as_ref(),
                &device_id,
                parent_chat.as_ref(),
                base,
            )
            .await?;
        let mut branch = args
            .branch
            .as_deref()
            .map(str::trim)
            .filter(|b| !b.is_empty())
            .map(str::to_owned);
        if let Some(placed) = &placement
            && (branch.is_none() || placed.mode != "parent")
        {
            branch = placed.branch.clone().or(branch);
        }

        let chat_id = uuid::Uuid::new_v4().to_string();
        let mut mutate = json!({
            "op": "createChat",
            "chatId": chat_id,
            "deviceId": device_id,
            "config": config,
        });
        if let Some(space) = &space {
            mutate["spaceId"] = json!(space.id);
        }
        if let Some(parent) = &parent_chat_id {
            mutate["parentChatId"] = json!(parent);
        }
        if args.notify {
            mutate["notifyParent"] = json!(true);
        }
        if let Some(branch) = &branch {
            mutate["branch"] = json!(branch);
        }
        let mut cwd = explicit_cwd.or_else(|| placement.as_ref().map(|p| p.path.clone()));
        // A projectless chat runs in its own scratch folder on a host that
        // creates them; naming it here keeps createChat and the first run
        // in agreement.
        if space.is_none()
            && cwd.is_none()
            && self.godspeed.devices().await?.iter().any(|d| {
                d.id == device_id
                    && d.capabilities
                        .iter()
                        .any(|c| c == zeron_proto::capabilities::PROJECTLESS_SCRATCH_V1)
            })
        {
            cwd = Some(zeron_proto::projectless::scratch_cwd(
                &chat_id,
                chrono::Utc::now(),
            ));
        }
        if let Some(cwd) = &cwd {
            mutate["cwd"] = json!(cwd);
        }
        if let Err(err) = self.godspeed.mutate(mutate).await {
            // A refused chat (e.g. the conductor's sub-agent cap) must not
            // leave the worktree made for it behind.
            if let Some(placed) = placement.as_ref().filter(|p| p.mode == "new") {
                let space = space.as_ref().expect("a new worktree has a project");
                let removed = self
                    .godspeed
                    .call(
                        zeron_rpc::methods::DELETE_WORKTREE,
                        json!({
                            "repoPath": space.path,
                            "worktreePath": placed.path,
                            "force": true,
                            "targetDeviceId": device_id,
                        }),
                    )
                    .await;
                return Err(match removed {
                    Ok(_) => err.context("the chat wasn't created, so its new worktree was removed"),
                    Err(cleanup) => err.context(format!(
                        "the chat wasn't created, and removing its new worktree at {} failed too: {cleanup}",
                        placed.path
                    )),
                });
            }
            return Err(err);
        }
        if let Some(title) = args
            .title
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            self.godspeed
                .mutate(json!({ "op": "renameChat", "chatId": chat_id, "title": title }))
                .await?;
        }

        let mut result = json!({
            "chatId": chat_id,
            "kind": kind,
            "deviceId": device_id,
            "project": space.as_ref().map(|s| json!({ "id": s.id, "name": s.display_name(), "path": s.path })),
            "harness": harness,
            "model": args.model,
            "reasoning": reasoning,
            "title": args.title,
            "parentChatId": parent_chat_id,
        });
        if let Some(placed) = &placement {
            result["branch"] = json!(branch);
            result["worktree"] = json!({
                "mode": placed.mode,
                "path": placed.path,
                "branch": placed.branch,
            });
            if let Some(error) = &placed.setup_error {
                result["worktree"]["setupError"] = json!(error);
            }
        }
        if let Some(prompt) = args.prompt.filter(|p| !p.trim().is_empty()) {
            // The row may not have folded into WatchChats yet; build the
            // chat locally from what we just wrote rather than re-reading.
            let chat = Chat {
                id: chat_id.clone(),
                device_id: device_id.clone(),
                title: args.title.clone(),
                archived: false,
                cwd: cwd.clone(),
                branch: branch.clone(),
                checkout_id: None,
                source_context: None,
                config: Some(config),
                last_message_preview: None,
                last_message_at: None,
                created_at: chrono::Utc::now(),
                harness_session_id: None,
                harness_session_cwd: None,
                parent_chat_id: parent_chat_id.clone(),
                limit: None,
                lifecycle: Default::default(),
                space_id: space.as_ref().map(|s| s.id.clone()),
                last_seen_at: None,
                room_gen: None,
            };
            let sent = self
                .deliver(&chat, space.as_ref(), &harnesses, None, prompt, "run")
                .await?;
            result["sent"] = sent;
            let pending = Arc::new(PendingTurn {
                baseline: None,
                // A new chat has no earlier messages.
                start: TurnStart::default(),
            });
            self.pending_turns
                .lock()
                .await
                .insert(chat.id.clone(), pending.clone());
            if args.wait {
                result["turn"] = self
                    .await_pending_turn(&chat, pending, wait_duration(args.timeout_secs))
                    .await?;
            }
        }
        Ok(result)
    }

    async fn read_chat(&self, args: ReadChatArgs) -> anyhow::Result<Value> {
        let chat = self.godspeed.resolve_chat(&args.chat).await?;
        let entries = self
            .godspeed
            .transcript_on(&chat.id, Some(&chat.device_id))
            .await?;
        let rendered = render_entries(
            &entries,
            RenderOptions {
                include_reasoning: args.include_reasoning,
                include_tools: args.include_tools.unwrap_or(true),
            },
        );
        let total = rendered.len();
        let limit = args.limit.unwrap_or(40).clamp(1, 500);
        let end = total.saturating_sub(args.offset);
        let start = end.saturating_sub(limit);
        let window = &rendered[start..end];
        Ok(json!({
            "chatId": chat.id,
            "title": chat.title,
            "total": total,
            "returned": window.len(),
            "olderRemaining": start,
            "newerSkipped": total - end,
            "pendingInput": last_pending_input(&rendered),
            "messages": window,
        }))
    }

    async fn send_message(&self, args: SendArgs) -> anyhow::Result<Value> {
        let text = args.text.trim();
        if text.is_empty() {
            anyhow::bail!("text is empty");
        }
        let chat = self.godspeed.resolve_chat(&args.chat).await?;
        if self.godspeed.origin().chat_id.as_deref() == Some(chat.id.as_str()) {
            anyhow::bail!(
                "refusing to send a message to your own chat ({})",
                short(&chat.id)
            );
        }
        let (spaces, sessions, harnesses) = tokio::try_join!(
            self.godspeed.spaces(),
            self.godspeed.sessions(),
            self.godspeed.harnesses_on(Some(&chat.device_id))
        )?;
        let space = chat
            .space_id
            .as_deref()
            .and_then(|id| spaces.iter().find(|s| s.id == id));
        let baseline = session_for(&sessions, &chat);
        let mode = args.mode.as_deref().unwrap_or("auto");
        let message_ids = self
            .godspeed
            .transcript_on(&chat.id, Some(&chat.device_id))
            .await?
            .into_iter()
            .map(|m| m.id)
            .collect();
        let body = self.attribute(&chat, text).await;
        let mut result = json!({
            "chatId": chat.id,
            "title": chat.title,
        });
        result["sent"] = self
            .deliver(&chat, space, &harnesses, baseline.as_ref(), body, mode)
            .await?;
        let pending = Arc::new(PendingTurn {
            baseline,
            start: TurnStart { message_ids },
        });
        self.pending_turns
            .lock()
            .await
            .insert(chat.id.clone(), pending.clone());
        if args.wait {
            result["turn"] = self
                .await_pending_turn(&chat, pending, wait_duration(args.timeout_secs))
                .await?;
        }
        Ok(result)
    }

    async fn wait_for_turn(&self, args: WaitArgs) -> anyhow::Result<Value> {
        let chat = self.godspeed.resolve_chat(&args.chat).await?;
        let pending = self.pending_turns.lock().await.get(&chat.id).cloned();
        let turn = match pending {
            Some(pending) => {
                self.await_pending_turn(&chat, pending, wait_duration(args.timeout_secs))
                    .await?
            }
            None => {
                self.await_turn(
                    &chat,
                    None,
                    false,
                    wait_duration(args.timeout_secs),
                    TurnStart::default(),
                )
                .await?
            }
        };
        Ok(json!({ "chatId": chat.id, "title": chat.title, "turn": turn }))
    }

    async fn archive_chat(&self, args: ArchiveArgs) -> anyhow::Result<Value> {
        let chat = self.godspeed.resolve_chat(&args.chat).await?;
        let archived = args.archived.unwrap_or(true);
        self.godspeed
            .mutate(json!({ "op": "setChatArchived", "chatId": chat.id, "archived": archived }))
            .await?;
        Ok(json!({ "chatId": chat.id, "title": chat.title, "archived": archived }))
    }

    async fn show_visual(&self, args: ShowVisualArgs) -> anyhow::Result<Value> {
        let chat_id = self
            .godspeed
            .origin()
            .chat_id
            .clone()
            .context("show_visual only works from an agent Godspeed launched in a chat")?;
        let assets = read_visual_assets(&args.assets).await?;
        let shown = self
            .godspeed
            .call(
                zeron_rpc::methods::PUBLISH_VISUAL,
                json!({
                    "chatId": chat_id,
                    "title": args.title,
                    "script": args.script,
                    "assets": assets,
                }),
            )
            .await?;
        Ok(json!({
            "visual": shown,
            "message": "Shown to the reader above your reply. Don't mention or describe the visual; reply with only what it doesn't already say.",
        }))
    }

    async fn show_image(&self, args: ShowImageArgs) -> anyhow::Result<Value> {
        use base64::Engine as _;
        let chat_id = self
            .godspeed
            .origin()
            .chat_id
            .clone()
            .context("show_image only works from an agent Godspeed launched in a chat")?;
        let bytes = read_image(&args.path).await?;
        let name = std::path::Path::new(&args.path)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "image".into());
        let shown = self
            .godspeed
            .call(
                zeron_rpc::methods::PUBLISH_IMAGE,
                json!({
                    "chatId": chat_id,
                    "name": name,
                    "data": base64::engine::general_purpose::STANDARD.encode(bytes),
                }),
            )
            .await?;
        Ok(json!({
            "image": shown,
            "message": "Shown to the reader above your reply. Don't mention or describe the image; reply with only what it doesn't already show.",
        }))
    }

    /// One `preview_*` tool: drive this chat's preview tab in the window.
    async fn preview(&self, name: &str, args: Value) -> Result<Value, String> {
        let chat_id = self.godspeed.origin().chat_id.clone().ok_or_else(|| {
            format!("{name} only works from an agent Godspeed launched in a chat")
        })?;
        let action = crate::preview::action(name, args)?;
        let value = self
            .godspeed
            .call(
                zeron_rpc::methods::PREVIEW_ACT,
                json!({ "chatId": chat_id, "action": action }),
            )
            .await
            .map_err(|e| {
                // The engine's message is for the agent; drop the method name.
                let message = format!("{e:#}");
                let prefix = format!("{}: ", zeron_rpc::methods::PREVIEW_ACT);
                message
                    .strip_prefix(&prefix)
                    .map(str::to_owned)
                    .unwrap_or(message)
            })?;
        Ok(crate::preview::present(&action, value))
    }

    async fn interrupt_chat(&self, args: ChatArgs) -> anyhow::Result<Value> {
        let chat = self.godspeed.resolve_chat(&args.chat).await?;
        let command_id = self
            .godspeed
            .queue_command(&chat.id, &SessionCommandPayload::Interrupt {})
            .await?;
        Ok(json!({ "chatId": chat.id, "commandId": command_id }))
    }

    async fn respond_to_input(&self, args: RespondArgs) -> anyhow::Result<Value> {
        let chat = self.godspeed.resolve_chat(&args.chat).await?;
        let entries = self
            .godspeed
            .transcript_on(&chat.id, Some(&chat.device_id))
            .await?;
        let rendered = render_entries(&entries, RenderOptions::default());
        let pending = match &args.request_id {
            Some(id) => rendered
                .iter()
                .rev()
                .filter_map(|m| m.pending_input.clone())
                .find(|p| p.get("requestId").and_then(Value::as_str) == Some(id.as_str())),
            None => last_pending_input(&rendered),
        };
        let request_id = match args.request_id {
            Some(id) => id,
            None => pending
                .as_ref()
                .and_then(|p| p.get("requestId").and_then(Value::as_str))
                .map(str::to_owned)
                .ok_or_else(|| {
                    anyhow::anyhow!("chat {} has no pending question", short(&chat.id))
                })?,
        };
        if args.answers.is_empty() {
            anyhow::bail!("answers is empty");
        }
        let questions: Vec<UserInputQuestion> = pending
            .and_then(|p| p.get("questions").cloned())
            .and_then(|q| serde_json::from_value(q).ok())
            .unwrap_or_default();
        let answers: Vec<UserInputAnswer> = args
            .answers
            .into_iter()
            .map(|a| UserInputAnswer {
                question_id: a.question_id,
                labels: a.labels,
            })
            .collect();
        // Approving is a grant of access: only as far as this agent's own
        // chat could go without asking. Declining is always allowed.
        //
        // Conductor rule, keyed on the chat being answered (and on the
        // answering chat): when either is a conductor member, an agent may
        // only approve a low-risk request once; questions and anything past
        // low risk wait for the user. A failed membership lookup refuses.
        let conductor_rule = self.conductor_rule_for(&chat.id).await;
        if conductor_rule.as_ref().map_or(true, |applies| *applies) {
            for answer in &answers {
                let is_approval = questions
                    .iter()
                    .find(|q| q.id == answer.question_id)
                    .is_some_and(UserInputQuestion::is_approval);
                if let Err(error) = &conductor_rule {
                    anyhow::ensure!(
                        is_approval,
                        "Couldn't check whether this chat belongs to a conductor ({error:#}), so its questions wait for the user."
                    );
                }
                anyhow::ensure!(
                    is_approval,
                    "Questions in a conductor wait for the user; don't answer them for another agent."
                );
            }
        }
        if questions.iter().any(UserInputQuestion::is_approval) {
            let mode = self.origin_access_mode().await?;
            for answer in &answers {
                let Some(approval) = questions
                    .iter()
                    .find(|q| q.id == answer.question_id)
                    .and_then(|q| q.approval.as_ref())
                else {
                    continue;
                };
                for label in &answer.labels {
                    let decision = approval.decision_for(label).ok_or_else(|| {
                        anyhow::anyhow!(
                            "{label:?} is not a decision for this approval; use one of: {}",
                            approval
                                .decisions
                                .iter()
                                .map(|d| d.label.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    })?;
                    if decision.approves() {
                        match &conductor_rule {
                            Err(error) => anyhow::bail!(
                                "Couldn't check whether this chat belongs to a conductor ({error:#}), so \"{}\" waits for the user.",
                                approval.title
                            ),
                            Ok(true) => {
                                let risk = approval_risk(approval, chat.cwd.as_deref());
                                anyhow::ensure!(
                                    risk == ApprovalRisk::Low,
                                    "\"{}\" is {}; in a conductor, agents approve low-risk requests only. Decline it or leave it for the user.",
                                    approval.title,
                                    risk.label().to_lowercase()
                                );
                                anyhow::ensure!(
                                    decision == ApprovalDecision::Approve,
                                    "In a conductor, agents approve one request at a time; use \"{}\" instead of \"{label}\".",
                                    approval
                                        .decisions
                                        .iter()
                                        .find(|o| o.decision == ApprovalDecision::Approve)
                                        .map_or("Approve", |o| o.label.as_str())
                                );
                            }
                            Ok(false) => {}
                        }
                    }
                    anyhow::ensure!(
                        !decision.approves() || mode.is_some_and(|m| m.covers(approval)),
                        "Your chat runs in {}, which doesn't allow approving \"{}\" for another agent. Decline it or leave it for the user.",
                        mode.map_or("no Godspeed chat", |m| m.label()),
                        approval.title
                    );
                }
            }
        }
        let command_id = self
            .godspeed
            .queue_command(
                &chat.id,
                &SessionCommandPayload::RespondInput {
                    request_id: request_id.clone(),
                    answers,
                },
            )
            .await?;
        Ok(json!({ "chatId": chat.id, "requestId": request_id, "commandId": command_id }))
    }

    /// Where a new chat in `space` works, per its `worktree` argument.
    /// `None` keeps the old default (the project folder, or a scratch folder
    /// without a project). A fresh worktree is created here, on the host,
    /// through the same path the app uses (environment, ports, setup).
    async fn place_chat(
        &self,
        choice: &WorktreeChoice,
        space: Option<&Space>,
        device_id: &str,
        parent: Option<&Chat>,
        base: Option<String>,
    ) -> anyhow::Result<Option<Placement>> {
        let Some(space) = space else {
            return Ok(None);
        };
        // The parent's checkout counts only when it is in this project on
        // this host; anywhere else it means nothing here.
        let parent_here = parent.filter(|p| {
            p.device_id == device_id && p.space_id.as_deref() == Some(space.id.as_str())
        });
        let unsupported = |err: anyhow::Error| {
            if err.to_string().contains("nknown method") {
                anyhow::anyhow!("device {device_id} runs an older Godspeed without worktree support for agents; update it there")
            } else {
                err
            }
        };
        match choice {
            WorktreeChoice::Parent => Ok(parent_here.and_then(|p| {
                Some(Placement {
                    mode: "parent",
                    path: p.cwd.clone()?,
                    branch: p.branch.clone(),
                    setup_error: None,
                })
            })),
            WorktreeChoice::Existing(target) => {
                let resolved: zeron_proto::ResolvedWorktree = serde_json::from_value(
                    self.godspeed
                        .call(
                            zeron_rpc::methods::RESOLVE_WORKTREE,
                            json!({ "repoPath": space.path, "target": target, "targetDeviceId": device_id }),
                        )
                        .await
                        .map_err(unsupported)?,
                )
                .context("ResolveWorktree reply")?;
                Ok(Some(Placement {
                    mode: "existing",
                    path: resolved.path,
                    branch: resolved.branch,
                    setup_error: None,
                }))
            }
            WorktreeChoice::New => {
                let base = match base {
                    Some(base) => base,
                    None => self.parent_base(space, device_id, parent_here).await,
                };
                let outcome: zeron_proto::CreateWorktreeOutcome = serde_json::from_value(
                    self.godspeed
                        .call(
                            zeron_rpc::methods::CREATE_WORKTREE,
                            json!({
                                "repoPath": space.path,
                                "branch": base,
                                "spaceId": space.id,
                                "targetDeviceId": device_id,
                            }),
                        )
                        .await
                        .map_err(unsupported)
                        .with_context(|| format!("couldn't create a worktree off {base}"))?,
                )
                .context("CreateWorktree reply")?;
                Ok(Some(Placement {
                    mode: "new",
                    path: outcome.worktree.path,
                    branch: Some(outcome.worktree.branch),
                    setup_error: outcome.setup_error,
                }))
            }
        }
    }

    /// What a new worktree starts from by default: the branch (or detached
    /// commit) the parent's checkout is on, else the project's checked-out
    /// branch (`HEAD` of its folder).
    async fn parent_base(&self, space: &Space, device_id: &str, parent: Option<&Chat>) -> String {
        let Some(parent) = parent else {
            return "HEAD".into();
        };
        if let Some(cwd) = parent.cwd.as_deref()
            && let Ok(reply) = self
                .godspeed
                .call(
                    zeron_rpc::methods::RESOLVE_WORKTREE,
                    json!({ "repoPath": space.path, "target": cwd, "targetDeviceId": device_id }),
                )
                .await
            && let Ok(resolved) = serde_json::from_value::<zeron_proto::ResolvedWorktree>(reply)
            && let Some(base) = resolved.branch.or(resolved.head)
        {
            return base;
        }
        parent.branch.clone().unwrap_or_else(|| "HEAD".into())
    }

    /// A chat may start sub-agents when it is top-level or a conductor member
    /// below the depth cap; otherwise `refusal` says why not.
    async fn ensure_may_parent(&self, chat: &Chat, refusal: &str) -> anyhow::Result<()> {
        if chat.parent_chat_id.is_none() {
            return Ok(());
        }
        match self.godspeed.conductor_membership(&chat.id).await? {
            Some(m) => {
                anyhow::ensure!(
                    can_spawn_at(m.depth),
                    "This conductor allows sub-agents at most {MAX_CONDUCTOR_DEPTH} levels below its main chat, and {} is at the deepest level. Do the work there instead.",
                    short(&chat.id)
                );
                Ok(())
            }
            None => anyhow::bail!("{refusal}"),
        }
    }

    /// Where the chat this server speaks for sits in a conductor; `None`
    /// outside a chat or a conductor. Errors when the lookup fails, so
    /// callers can fail closed.
    async fn origin_conductor_membership(
        &self,
    ) -> anyhow::Result<Option<zeron_proto::ConductorMembership>> {
        match self.godspeed.origin().chat_id.as_deref() {
            Some(origin) => self.godspeed.conductor_membership(origin).await,
            None => Ok(None),
        }
    }

    /// Whether the conductor rule governs answering `target`: it or the
    /// answering chat belongs to a conductor. `Err` when that can't be told.
    async fn conductor_rule_for(&self, target: &str) -> anyhow::Result<bool> {
        if self.godspeed.conductor_membership(target).await?.is_some() {
            return Ok(true);
        }
        Ok(self.origin_conductor_membership().await?.is_some())
    }

    /// The conductor the calling chat belongs to.
    async fn origin_conductor(&self) -> anyhow::Result<(zeron_proto::Conductor, u8)> {
        let origin = self
            .godspeed
            .origin()
            .chat_id
            .clone()
            .context("conductor tools need a Godspeed chat")?;
        let membership = self
            .godspeed
            .conductor_membership(&origin)
            .await?
            .context("this chat doesn't belong to a conductor")?;
        let conductor = self.godspeed.conductor(&membership.conductor_id).await?;
        Ok((conductor, membership.depth))
    }

    async fn conductor_info(&self) -> anyhow::Result<Value> {
        let (conductor, depth) = self.origin_conductor().await?;
        let spaces = self.godspeed.spaces().await?;
        let sessions = self.godspeed.sessions().await?;
        let chats = self.godspeed.chats().await?;
        let repos: Vec<Value> = conductor
            .space_ids
            .iter()
            .filter_map(|id| spaces.iter().find(|s| &s.id == id))
            .map(|s| json!({ "space": s.id, "path": s.path, "name": s.display_name() }))
            .collect();
        let sub_agents: Vec<Value> = self
            .godspeed
            .conductor_tree(&conductor.id)
            .await?
            .into_iter()
            .filter(|node| node.depth > 0 && !node.archived)
            .map(|node| {
                let status = chats
                    .iter()
                    .find(|c| c.id == node.chat_id)
                    .map(|chat| status_of(session_for(&sessions, chat).as_ref()).0)
                    .unwrap_or_else(|| "idle".into());
                json!({
                    "chatId": node.chat_id,
                    "title": node.title,
                    "depth": node.depth,
                    "parentChatId": node.parent_chat_id,
                    "status": status,
                })
            })
            .collect();
        Ok(json!({
            "conductor": {
                "id": conductor.id,
                "name": conductor.name,
                "purpose": conductor.purpose,
                "folder": conductor.folder,
                "mainChatId": conductor.main_chat_id,
            },
            "yourDepth": depth,
            "mayStartSubAgents": can_spawn_at(depth),
            "repos": repos,
            "subAgents": sub_agents,
        }))
    }

    async fn conductor_notes_read(&self) -> anyhow::Result<Value> {
        let (conductor, _) = self.origin_conductor().await?;
        let reply = self
            .godspeed
            .call(
                zeron_rpc::methods::GET_CONDUCTOR_NOTES,
                json!({ "id": conductor.id }),
            )
            .await?;
        Ok(json!({ "notes": reply["notes"] }))
    }

    async fn conductor_notes_write(&self, args: NotesArgs) -> anyhow::Result<Value> {
        anyhow::ensure!(
            args.notes.len() <= MAX_NOTES_BYTES,
            "Notes are {} bytes; the limit is {} KB. Trim them and write again.",
            args.notes.len(),
            MAX_NOTES_BYTES / 1024
        );
        let (conductor, _) = self.origin_conductor().await?;
        self.godspeed
            .call(
                zeron_rpc::methods::SET_CONDUCTOR_NOTES,
                json!({ "id": conductor.id, "notes": args.notes }),
            )
            .await?;
        Ok(json!({ "saved": true, "bytes": args.notes.len() }))
    }

    /// The access mode of the chat this server speaks for; `None` outside a
    /// chat, where nothing grants approvals.
    async fn origin_access_mode(&self) -> anyhow::Result<Option<AccessMode>> {
        let Some(origin) = self.godspeed.origin().chat_id.clone() else {
            return Ok(None);
        };
        let chat = self.godspeed.resolve_chat(&origin).await?;
        Ok(Some(
            chat.config
                .as_ref()
                .map(ChatConfig::effective_access_mode)
                .unwrap_or_default(),
        ))
    }

    // ---- shared pieces -------------------------------------------------------

    /// Prefix the sender's identity when this server speaks for a chat, so
    /// the receiving agent (and the human reading that transcript) can tell
    /// an agent-to-agent message from a typed one. Reply instructions always
    /// use the full id: display prefixes need not uniquely identify a chat.
    async fn attribute(&self, target: &Chat, text: &str) -> String {
        let Some(origin_id) = self.godspeed.origin().chat_id.as_deref() else {
            return text.to_owned();
        };
        if origin_id == target.id {
            return text.to_owned();
        }
        let title = match self.godspeed.resolve_chat(origin_id).await {
            Ok(chat) => chat.title,
            Err(_) => None,
        };
        let label = match title {
            Some(title) if !title.trim().is_empty() => {
                format!("{} ({})", title.trim(), short(origin_id))
            }
            _ => short(origin_id).to_owned(),
        };
        zeron_proto::agent_messages::agent_message(&label, origin_id, text)
    }

    /// Pick and perform the delivery the composer would.
    async fn deliver(
        &self,
        chat: &Chat,
        space: Option<&Space>,
        harnesses: &[HarnessInfo],
        session: Option<&Session>,
        text: String,
        mode: &str,
    ) -> anyhow::Result<Value> {
        let harness = chat
            .config
            .as_ref()
            .map(|c| c.harness)
            .map_or_else(|| default_harness(harnesses), Ok)?;
        let (status, _) = status_of(session);
        // A quiet tool call can outlive the UI's stale-status window. Route
        // through steering and let the host decide whether a live run exists.
        let live = session.map(|s| s.status).unwrap_or(SessionStatus::Idle);
        let chosen = match mode {
            "auto" => match live {
                SessionStatus::Idle | SessionStatus::Errored => "run",
                SessionStatus::Working => "steer",
                SessionStatus::AwaitingInput => anyhow::bail!(
                    "chat {} is waiting for an answer; use respond_to_input (or mode 'queue' to hold this message for after the turn)",
                    short(&chat.id)
                ),
            },
            "run" | "steer" | "queue" => mode,
            other => anyhow::bail!("unknown mode {other:?}; expected auto, run, steer, or queue"),
        };
        let id = match chosen {
            "run" => {
                let config = chat.config.clone();
                let cwd = chat
                    .cwd
                    .clone()
                    .or_else(|| space.map(|s| s.path.clone()))
                    .unwrap_or_else(|| "~".into());
                let request = RunRequest {
                    mcp: None,
                    prompt: text,
                    harness: Some(harness),
                    model: config.as_ref().and_then(|c| c.model.clone()),
                    reasoning: config.as_ref().and_then(|c| c.reasoning),
                    model_options: config
                        .as_ref()
                        .map(|c| c.model_options.clone())
                        .unwrap_or_default(),
                    cwd,
                    sandbox: config
                        .as_ref()
                        .map(|c| c.sandbox)
                        .unwrap_or(SandboxLevel::WorkspaceWrite),
                    auto_approve: false,
                    resume: None,
                    attachments: Vec::new(),
                    worktree: None,
                    access: None,
                    env: Default::default(),
                };
                let mut request = request;
                if let Some(config) = &config {
                    request.set_access_mode(config.effective_access_mode());
                }
                self.godspeed
                    .queue_command(
                        &chat.id,
                        &SessionCommandPayload::Run {
                            request,
                            message_id: uuid::Uuid::new_v4().to_string(),
                        },
                    )
                    .await?
            }
            "steer" => {
                self.godspeed
                    .queue_command(
                        &chat.id,
                        &SessionCommandPayload::Steer {
                            prompt: text,
                            message_id: Some(uuid::Uuid::new_v4().to_string()),
                        },
                    )
                    .await?
            }
            _ => self.godspeed.queue_message(&chat.id, &text).await?,
        };
        Ok(json!({
            "delivery": chosen,
            "id": id,
            "chatStatusAtSend": status,
        }))
    }

    async fn await_pending_turn(
        &self,
        chat: &Chat,
        pending: Arc<PendingTurn>,
        timeout: Duration,
    ) -> anyhow::Result<Value> {
        let turn = self
            .await_turn(
                chat,
                pending.baseline.as_ref(),
                true,
                timeout,
                pending.start.clone(),
            )
            .await?;
        if turn["timedOut"] == false {
            let mut turns = self.pending_turns.lock().await;
            // A concurrent send may already have installed a newer baseline.
            if turns
                .get(&chat.id)
                .is_some_and(|p| Arc::ptr_eq(p, &pending))
            {
                turns.remove(&chat.id);
            }
        }
        Ok(turn)
    }

    /// Wait, then report the outcome with the assistant messages that
    /// are not in `start`'s baseline.
    async fn await_turn(
        &self,
        chat: &Chat,
        baseline: Option<&Session>,
        expect_turn: bool,
        timeout: Duration,
        start: TurnStart,
    ) -> anyhow::Result<Value> {
        let deadline = Instant::now() + timeout;
        let (mut outcome, session) = self
            .godspeed
            .wait_for_turn(chat, baseline, expect_turn, timeout)
            .await?;
        // Registry session updates can arrive before the separate transcript doc.
        // Keep the same deadline while waiting for a response to this send.
        let mut rendered = Vec::new();
        let mut replies = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                if outcome == TurnOutcome::Completed && expect_turn {
                    outcome = TurnOutcome::TimedOut;
                }
                break;
            }
            let entries = match tokio::time::timeout(
                remaining,
                self.godspeed.transcript_on(&chat.id, Some(&chat.device_id)),
            )
            .await
            {
                Ok(entries) => entries?,
                Err(_) => {
                    outcome = TurnOutcome::TimedOut;
                    break;
                }
            };
            rendered = render_entries(&entries, RenderOptions::default());
            replies = rendered
                .iter()
                .filter(|m| m.role == zeron_doc::MessageRole::Assistant)
                .filter(|m| !start.message_ids.contains(&m.id))
                .cloned()
                .collect();
            if !expect_turn
                || outcome != TurnOutcome::Completed
                || replies
                    .iter()
                    .any(|m| m.status != Some(zeron_doc::MessageStatus::Streaming))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100).min(remaining)).await;
        }
        let (status, _) = status_of(session.as_ref());
        Ok(json!({
            "outcome": outcome,
            "status": status,
            "timedOut": outcome == TurnOutcome::TimedOut,
            "pendingInput": last_pending_input(&rendered),
            "replies": replies,
        }))
    }
}

/// claude-code when it is offered here, else the first available harness.
fn default_harness(harnesses: &[HarnessInfo]) -> anyhow::Result<HarnessId> {
    harnesses
        .iter()
        .find(|h| h.id == HarnessId::ClaudeCode && h.available())
        .or_else(|| {
            harnesses
                .iter()
                .find(|h| h.available() && h.id != HarnessId::Mock)
        })
        .map(|h| h.id)
        .ok_or_else(|| {
            anyhow::anyhow!("no harness is available on this device (see list_harnesses)")
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::godspeed::Origin;
    use async_trait::async_trait;
    use futures::StreamExt;
    use std::sync::Mutex;
    use zeron_rpc::{RpcError, RpcReply, RpcService, memory_client, methods};

    /// Two devices with distinct catalogs and repeated project paths, plus
    /// two chats with a two-message transcript. Writes are recorded for assertions.
    #[derive(Default)]
    struct World {
        writes: Mutex<Vec<(String, Value)>>,
        dispatch_barrier: Option<tokio::sync::Barrier>,
        beta_parent: Option<String>,
        beta_remote: bool,
        extra_chats: Vec<Value>,
        catalog_error: Option<&'static str>,
        reads: Mutex<Vec<(String, Value)>>,
        /// Replaces the default two-message transcript.
        transcript: Option<Value>,
        /// Chats that belong to the fake conductor `cond-1`: (chat id, depth).
        conductor: Vec<(&'static str, u8)>,
        /// Conductor membership lookups fail (engine trouble).
        membership_error: bool,
        notes: Mutex<String>,
    }

    fn stream(item: Value) -> RpcReply {
        RpcReply::Stream(futures::stream::iter(vec![item]).boxed())
    }

    #[async_trait]
    impl RpcService for World {
        async fn handle(&self, method: &str, params: Value) -> Result<RpcReply, RpcError> {
            if matches!(method, methods::LIST_HARNESSES | methods::LIST_MODELS) {
                self.reads
                    .lock()
                    .unwrap()
                    .push((method.into(), params.clone()));
                if self.catalog_error == Some(method) {
                    return Err(RpcError::Failed("catalog unavailable".into()));
                }
            }
            let remote = params["targetDeviceId"] == "dev-remote";
            Ok(match method {
                methods::LOCAL_DEVICE => RpcReply::Value(json!({ "deviceId": "dev-local" })),
                methods::ENGINE_INFO => RpcReply::Value(json!({
                    "deviceId": "dev-local", "workspaceScope": "local"
                })),
                methods::WATCH_DEVICES => stream(json!([{
                    "id": "dev-local", "name": "Laptop", "platform": "linux",
                    "lastSeenAt": null
                }, {"id": "dev-remote", "name": "Worker", "platform": "linux", "lastSeenAt": null}])),
                methods::WATCH_SPACES => stream(json!([{
                    "id": "space-1", "deviceId": "dev-local", "path": "/repo/comet",
                    "gitDetected": true, "createdAt": "2026-09-01T00:00:00Z"
                }, {
                    "id": "space-remote", "deviceId": "dev-remote", "path": "/repo/comet",
                    "gitDetected": true, "createdAt": "2026-09-01T00:00:00Z"
                }, {
                    "id": "space-unique", "deviceId": "dev-remote", "path": "/repo/unique",
                    "gitDetected": true, "createdAt": "2026-09-01T00:00:00Z"
                }])),
                methods::WATCH_CHATS => {
                    let mut chats = json!([
                    {
                        "id": "chat-alpha-1", "deviceId": "dev-local", "title": "Alpha",
                        "archived": false, "spaceId": "space-1", "cwd": "/repo/comet",
                        "config": { "harness": "claude-code", "model": "opus", "reasoning": null, "sandbox": "workspace-write" },
                        "createdAt": "2026-09-01T00:00:00Z"
                    },
                    {
                        "id": "chat-beta-2", "deviceId": if self.beta_remote { "dev-remote" } else { "dev-local" }, "title": "Beta",
                        "parentChatId": self.beta_parent,
                        "archived": false, "spaceId": "space-1",
                        "createdAt": "2026-09-02T00:00:00Z"
                    }
                    ]);
                    chats
                        .as_array_mut()
                        .unwrap()
                        .extend(self.extra_chats.clone());
                    stream(chats)
                }
                methods::WATCH_SESSIONS => stream(json!([])),
                methods::LIST_HARNESSES if remote => RpcReply::Value(json!([
                    {"id":"codex", "name":"Remote Codex", "installed":true},
                    {"id":"cursor", "name":"Disabled Cursor", "installed":true, "enabled":false}
                ])),
                methods::LIST_HARNESSES => RpcReply::Value(json!([
                    { "id": "claude-code", "name": "Claude Code", "supportsSteering": true,
                      "steeringMode": "step-boundary", "reasoningLevels": [], "installed": true, "enabled": true },
                    { "id": "codex", "name": "Codex", "supportsSteering": true,
                      "steeringMode": "turn-boundary", "reasoningLevels": [], "installed": false, "enabled": true }
                ])),
                methods::LIST_MODELS if remote => RpcReply::Value(json!([
                    {"id":"remote-model", "label":"Remote model"}
                ])),
                methods::LIST_MODELS => RpcReply::Value(json!([
                    { "id": "opus", "label": "Opus" }, { "id": "sonnet", "label": "Sonnet" }
                ])),
                methods::WATCH_DOC_MESSAGES if self.transcript.is_some() => {
                    stream(json!({ "reset": self.transcript.clone().unwrap() }))
                }
                methods::WATCH_DOC_MESSAGES => stream(json!({ "reset": [
                    { "id": "u1", "role": "user", "createdAt": 1, "deviceId": "dev-local",
                      "parts": [{ "kind": "text", "id": "t", "text": "hi" }] },
                    { "id": "a1", "role": "assistant", "createdAt": 2, "deviceId": "dev-local",
                      "status": "complete",
                      "parts": [{ "kind": "text", "id": "t", "text": "hello back" }] }
                ]})),
                methods::PUBLISH_VISUAL => {
                    self.writes
                        .lock()
                        .unwrap()
                        .push((method.to_owned(), params.clone()));
                    RpcReply::Value(json!({ "id": "v:1", "title": params["title"] }))
                }
                methods::PREVIEW_ACT => {
                    self.writes
                        .lock()
                        .unwrap()
                        .push((method.to_owned(), params.clone()));
                    match params["action"]["action"].as_str() {
                        Some("screenshot") => RpcReply::Value(json!({
                            "png": "iVBORw0KGgo=", "width": 640, "height": 400,
                            "url": "http://localhost:3000/"
                        })),
                        Some("snapshot") => RpcReply::Value(json!({
                            "title": "Todo", "url": "http://localhost:3000/",
                            "outline": "button \"Add\" [ref=e1]"
                        })),
                        Some("click") => {
                            return Err(RpcError::Failed(
                                zeron_proto::preview_agent::PREVIEW_NEEDS_WINDOW.into(),
                            ));
                        }
                        _ => RpcReply::Value(json!({ "ok": true })),
                    }
                }
                methods::PUBLISH_IMAGE => {
                    self.writes
                        .lock()
                        .unwrap()
                        .push((method.to_owned(), params.clone()));
                    RpcReply::Value(json!({ "id": "img:1", "name": params["name"] }))
                }
                methods::MUTATE | methods::QUEUE_COMMAND | methods::QUEUE_MESSAGE => {
                    self.writes
                        .lock()
                        .unwrap()
                        .push((method.to_owned(), params));
                    if method == methods::QUEUE_COMMAND
                        && let Some(barrier) = &self.dispatch_barrier
                    {
                        // No dispatch can finish until both chats have work.
                        // A sequential batch deadlocks here, before any waits.
                        barrier.wait().await;
                    }
                    RpcReply::Value(json!({ "commandId": "cmd-1", "id": "q-1" }))
                }
                methods::GET_CONDUCTOR_MEMBERSHIP => {
                    if self.membership_error {
                        return Err(RpcError::Failed("registry unavailable".into()));
                    }
                    let chat = params["chatId"].as_str().unwrap_or_default();
                    RpcReply::Value(match self.conductor.iter().find(|(id, _)| *id == chat) {
                        Some((_, depth)) => json!({ "conductorId": "cond-1", "depth": depth }),
                        None => Value::Null,
                    })
                }
                methods::LIST_CONDUCTORS => RpcReply::Value(json!([{
                    "id": "cond-1", "name": "Acme", "folder": "/repo",
                    "deviceId": "dev-local", "mainChatId": "chat-boss-3",
                    "spaceIds": ["space-1"], "notes": "", "archived": false,
                    "createdAt": "2026-09-01T00:00:00Z"
                }])),
                methods::GET_CONDUCTOR_TREE => RpcReply::Value(json!([
                    { "chatId": "chat-boss-3", "depth": 0, "archived": false },
                    { "chatId": "chat-alpha-1", "parentChatId": "chat-boss-3", "depth": 1,
                      "title": "Alpha", "archived": false }
                ])),
                methods::GET_CONDUCTOR_NOTES => {
                    RpcReply::Value(json!({ "notes": self.notes.lock().unwrap().clone() }))
                }
                methods::SET_CONDUCTOR_NOTES => {
                    *self.notes.lock().unwrap() =
                        params["notes"].as_str().unwrap_or_default().to_owned();
                    RpcReply::Value(json!({}))
                }
                other => return Err(RpcError::UnknownMethod(other.into())),
            })
        }
    }

    fn tools(world: Arc<World>, origin: Origin) -> Tools {
        let client = memory_client(world);
        Tools::new(Arc::new(Godspeed::with_client(client, origin)))
    }

    #[tokio::test]
    async fn catalog_is_well_formed() {
        let defs = catalog();
        let mut names: Vec<&str> = defs.iter().map(|d| d.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), defs.len(), "tool names must be unique");
        for def in &defs {
            assert_eq!(def.input_schema["type"], "object", "{}", def.name);
            assert!(!def.description.is_empty());
        }
    }

    #[test]
    fn create_chat_notify_defaults_off_and_is_exposed_for_batch_items() {
        let default: CreateChatArgs = serde_json::from_value(json!({})).unwrap();
        let enabled: CreateChatArgs = serde_json::from_value(json!({ "notify": true })).unwrap();
        assert!(!default.notify);
        assert!(enabled.notify);
        let create_chat = catalog()
            .into_iter()
            .find(|tool| tool.name == "create_chat")
            .unwrap();
        assert_eq!(
            create_chat.input_schema["properties"]["notify"]["default"],
            false
        );
        let create_chats = catalog()
            .into_iter()
            .find(|tool| tool.name == "create_chats")
            .unwrap();
        assert!(
            create_chats.input_schema["properties"]["requests"]["items"]["properties"]["notify"]
                .is_object()
        );
    }

    #[test]
    fn worktree_is_documented_for_single_and_batch_creation() {
        assert_eq!(WorktreeChoice::parse(None), WorktreeChoice::Parent);
        assert_eq!(WorktreeChoice::parse(Some(" Parent ")), WorktreeChoice::Parent);
        assert_eq!(WorktreeChoice::parse(Some("NEW")), WorktreeChoice::New);
        assert_eq!(
            WorktreeChoice::parse(Some("feat/x")),
            WorktreeChoice::Existing("feat/x".into())
        );
        for name in ["create_chat", "create_chats"] {
            let tool = catalog().into_iter().find(|tool| tool.name == name).unwrap();
            assert!(tool.description.contains("worktree \"new\""), "{name}");
            let props = if name == "create_chat" {
                tool.input_schema["properties"].clone()
            } else {
                tool.input_schema["properties"]["requests"]["items"]["properties"].clone()
            };
            let doc = props["worktree"]["description"].as_str().unwrap();
            assert!(doc.contains("\"new\"") && doc.contains("\"parent\""), "{doc}");
            assert!(props["base"].is_object());
        }
    }

    #[tokio::test]
    async fn worktree_without_a_project_is_refused_before_any_write() {
        let world = Arc::new(World::default());
        let tools = tools(world.clone(), Origin::default());
        let err = tools
            .call("create_chat", json!({ "worktree": "new" }))
            .await
            .unwrap_err();
        assert!(err.contains("needs a project"), "{err}");
        let err = tools
            .call("create_chat", json!({ "project": "space-1", "worktree": "new", "cwd": "/x" }))
            .await
            .unwrap_err();
        assert!(err.contains("can't be combined"), "{err}");
        assert!(world.writes.lock().unwrap().is_empty());
    }

    #[test]
    fn the_show_visual_example_passes_its_own_check() {
        let start = SHOW_VISUAL_DESCRIPTION.find("local s = ui.state").unwrap();
        let end = start
            + SHOW_VISUAL_DESCRIPTION[start..]
                .find("\n\nGodspeed runs the script")
                .unwrap();
        let report = zeron_visual::check(&SHOW_VISUAL_DESCRIPTION[start..end], 1).unwrap();
        assert_eq!(report.interactions, 2, "both tabs");
    }

    #[tokio::test]
    async fn show_visual_is_offered_only_inside_a_chat() {
        let world = Arc::new(World::default());
        let outside = tools(world.clone(), Origin::default());
        assert!(!outside.has("show_visual"));
        assert!(
            outside
                .call(
                    "show_visual",
                    json!({ "script": "return ui.text 'x'", "title": "x" })
                )
                .await
                .is_err()
        );

        let inside = tools(
            world.clone(),
            Origin {
                chat_id: Some("chat-alpha-1".into()),
                device_id: Some("dev-local".into()),
            },
        );
        assert!(inside.has("show_visual"));
        // The guidance names every color the runtime accepts.
        for name in zeron_visual::TOKEN_NAMES {
            assert!(SHOW_VISUAL_DESCRIPTION.contains(name), "{name}");
        }
        let shown = inside
            .call(
                "show_visual",
                json!({ "script": "return ui.text 'chart'", "title": "Chart" }),
            )
            .await
            .unwrap();
        assert_eq!(shown["visual"]["title"], "Chart");
        let writes = world.writes.lock().unwrap();
        let (method, params) = writes.last().unwrap();
        assert_eq!(method, methods::PUBLISH_VISUAL);
        assert_eq!(params["chatId"], "chat-alpha-1");
        assert_eq!(params["script"], "return ui.text 'chart'");
    }

    #[tokio::test]
    async fn show_image_sends_the_file_only_from_inside_a_chat() {
        use base64::Engine as _;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("before.png");
        std::fs::write(&file, b"\x89PNG\r\n\x1a\nfixture").unwrap();
        let path = file.to_string_lossy().into_owned();
        let world = Arc::new(World::default());
        let outside = tools(world.clone(), Origin::default());
        assert!(!outside.has("show_image"));
        assert!(
            outside
                .call("show_image", json!({ "path": path }))
                .await
                .is_err()
        );

        let inside = tools(
            world.clone(),
            Origin {
                chat_id: Some("chat-alpha-1".into()),
                device_id: Some("dev-local".into()),
                ..Default::default()
            },
        );
        assert!(inside.has("show_image"));
        let shown = inside
            .call("show_image", json!({ "path": path }))
            .await
            .unwrap();
        assert_eq!(shown["image"]["name"], "before.png");
        {
            let writes = world.writes.lock().unwrap();
            let (method, params) = writes.last().unwrap();
            assert_eq!(method, methods::PUBLISH_IMAGE);
            assert_eq!(params["chatId"], "chat-alpha-1");
            let data = base64::engine::general_purpose::STANDARD
                .decode(params["data"].as_str().unwrap())
                .unwrap();
            assert_eq!(data, b"\x89PNG\r\n\x1a\nfixture");
        }

        let missing = inside
            .call("show_image", json!({ "path": "/nonexistent/shot.png" }))
            .await
            .unwrap_err();
        assert!(
            missing.contains("can't read /nonexistent/shot.png"),
            "{missing}"
        );
        let huge = dir.path().join("huge.png");
        std::fs::File::create(&huge)
            .unwrap()
            .set_len(zeron_proto::IMAGE_MAX_BYTES as u64 + 1)
            .unwrap();
        let error = inside
            .call("show_image", json!({ "path": huge.to_string_lossy() }))
            .await
            .unwrap_err();
        assert!(error.contains("over 10 MB"), "{error}");
        assert_eq!(
            world.writes.lock().unwrap().len(),
            1,
            "nothing else was sent"
        );
    }

    #[tokio::test]
    async fn list_and_read_chats() {
        let world = Arc::new(World::default());
        let tools = tools(world, Origin::default());
        let listed = tools.call("list_chats", json!({})).await.unwrap();
        assert_eq!(listed["total"], 2);
        // Newest activity first: Beta was created later.
        assert_eq!(listed["chats"][0]["title"], "Beta");
        assert_eq!(listed["chats"][1]["project"]["name"], "comet");
        assert_eq!(listed["chats"][1]["status"], "idle");

        let read = tools
            .call("read_chat", json!({ "chat": "alpha" }))
            .await
            .unwrap();
        assert_eq!(read["total"], 2);
        assert_eq!(read["messages"][1]["text"], "hello back");

        let read = tools
            .call("read_chat", json!({ "chat": "chat-alpha", "limit": 1 }))
            .await
            .unwrap();
        assert_eq!(read["returned"], 1);
        assert_eq!(read["olderRemaining"], 1);
        assert_eq!(read["messages"][0]["id"], "a1");
    }

    #[tokio::test]
    async fn send_attributes_and_refuses_self() {
        let world = Arc::new(World::default());
        let tools = tools(
            world.clone(),
            Origin {
                chat_id: Some("chat-beta-2".into()),
                device_id: Some("dev-local".into()),
            },
        );
        let err = tools
            .call("send_message", json!({ "chat": "beta", "text": "loop" }))
            .await
            .unwrap_err();
        assert!(err.contains("own chat"), "{err}");

        let sent = tools
            .call(
                "send_message",
                json!({ "chat": "alpha", "text": "please review" }),
            )
            .await
            .unwrap();
        assert_eq!(sent["sent"]["delivery"], "run");
        let writes = world.writes.lock().unwrap();
        let (method, params) = writes.last().expect("a queued command");
        assert_eq!(method, methods::QUEUE_COMMAND);
        assert_eq!(params["chatId"], "chat-alpha-1");
        assert_eq!(params["command"]["kind"], "run");
        let prompt = params["command"]["request"]["prompt"].as_str().unwrap();
        assert!(
            prompt.starts_with("[Message from Godspeed chat Beta (chat-bet)"),
            "{prompt}"
        );
        assert!(prompt.ends_with("please review"));
        assert!(prompt.contains("tool, chat chat-beta-2.]"), "{prompt}");
        assert_eq!(params["command"]["request"]["cwd"], "/repo/comet");
        assert_eq!(params["command"]["request"]["harness"], "claude-code");
        assert_eq!(params["command"]["request"]["model"], "opus");
    }

    #[tokio::test]
    async fn replies_use_the_full_id_with_multiple_voice_orchestrators() {
        let first = "voice-orchestrator-11111111-1111-4111-8111-111111111111";
        let second = "voice-orchestrator-22222222-2222-4222-8222-222222222222";
        let world = Arc::new(World {
            extra_chats: [first, second]
                .into_iter()
                .map(|id| {
                    json!({
                        "id": id, "deviceId": "dev-local", "title": "Voice session",
                        "archived": false, "createdAt": "2026-10-01T00:00:00Z"
                    })
                })
                .collect(),
            ..Default::default()
        });
        let orchestrator = tools(
            world.clone(),
            Origin {
                chat_id: Some(second.into()),
                device_id: None,
            },
        );
        let error = orchestrator
            .godspeed
            .resolve_chat("voice-or")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("2 chats start with"), "{error}");
        orchestrator
            .call(
                "send_message",
                json!({
                    "chat": "alpha", "text": "please review"
                }),
            )
            .await
            .unwrap();
        let reply_id = {
            let writes = world.writes.lock().unwrap();
            let prompt = writes.last().unwrap().1["command"]["request"]["prompt"]
                .as_str()
                .unwrap();
            prompt
                .split_once("tool, chat ")
                .unwrap()
                .1
                .split_once(".]\n\n")
                .unwrap()
                .0
                .to_owned()
        };
        assert_eq!(reply_id, second);
        let worker = tools(
            world.clone(),
            Origin {
                chat_id: Some("chat-alpha-1".into()),
                device_id: None,
            },
        );
        worker
            .call(
                "send_message",
                json!({
                    "chat": reply_id, "text": "review complete"
                }),
            )
            .await
            .unwrap();
        let writes = world.writes.lock().unwrap();
        assert_eq!(writes.last().unwrap().1["chatId"], second);
    }

    #[tokio::test]
    async fn auto_steers_busy_chats_even_at_turn_boundaries_or_after_long_quiet_tools() {
        let world = Arc::new(World::default());
        let tools = tools(world.clone(), Origin::default());
        let chats = tools.godspeed.chats().await.unwrap();
        let session = Session {
            chat_id: chats[0].id.clone(),
            device_id: "dev-local".into(),
            status: SessionStatus::Working,
            started_at: None,
            updated_at: chrono::Utc::now() - chrono::Duration::minutes(10),
            last_completed_turn: None,
            running_subagents: 0,
            background_tasks: 0,
            awaiting_approval: false,
        };
        // No mid-turn capability is required for a live mailbox delivery.
        let sent = tools
            .deliver(
                &chats[0],
                None,
                &[],
                Some(&session),
                "follow up".into(),
                "auto",
            )
            .await
            .unwrap();
        assert_eq!(sent["delivery"], "steer");
        let writes = world.writes.lock().unwrap();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].0, methods::QUEUE_COMMAND);
        assert_eq!(writes[0].1["command"]["kind"], "steer");
    }

    /// A worker chat parked on a command approval, and an orchestrator chat
    /// in `mode` speaking through this server.
    fn approval_world(mode: &str) -> Arc<World> {
        approval_world_in(mode, Vec::new())
    }

    /// [`approval_world`] with the boss chat (and any others listed) inside
    /// the fake conductor.
    fn approval_world_in(mode: &str, conductor: Vec<(&'static str, u8)>) -> Arc<World> {
        Arc::new(approval_world_parts(mode, conductor))
    }

    fn approval_world_parts(mode: &str, conductor: Vec<(&'static str, u8)>) -> World {
        use zeron_proto::ApprovalDecision as D;
        let ls = {
            let mut request =
                zeron_proto::ApprovalRequest::new(zeron_proto::ApprovalKind::Command, "List files")
                    .with_decisions(&[
                        D::Approve,
                        D::ApproveForSession,
                        D::ApproveAlways,
                        D::Decline,
                    ]);
            request.command = Some("ls".into());
            request.cwd = Some("/repo/comet".into());
            request.into_question("q-3")
        };
        let pick = json!({
            "id": "q-4", "header": "Database", "question": "Which database?",
            "options": ["Postgres", "SQLite"]
        });
        let question =
            zeron_proto::ApprovalRequest::new(zeron_proto::ApprovalKind::Command, "Run the deploy")
                .into_question("q-1");
        let edit = zeron_proto::ApprovalRequest::new(
            zeron_proto::ApprovalKind::FileChange,
            "Edit src/lib.rs",
        )
        .into_question("q-2");
        World {
            extra_chats: vec![json!({
                "id": "chat-boss-3", "deviceId": "dev-local", "title": "Boss",
                "archived": false, "spaceId": "space-1",
                "config": { "harness": "claude-code", "model": null, "reasoning": null,
                            "sandbox": "workspace-write", "accessMode": mode },
                "createdAt": "2026-09-03T00:00:00Z"
            })],
            transcript: Some(json!([
                { "id": "a1", "role": "assistant", "createdAt": 2, "deviceId": "dev-local",
                  "status": "streaming",
                  "parts": [{ "kind": "input", "id": "in-req-1", "requestId": "req-1", "questions": [question, edit, ls, pick],
                              "resolved": false }] }
            ])),
            conductor,
            ..World::default()
        }
    }

    #[tokio::test]
    async fn orchestrators_approve_only_within_their_own_access_mode() {
        let origin = Origin {
            chat_id: Some("chat-boss-3".into()),
            device_id: Some("dev-local".into()),
        };
        let answer = |question: &str, label: &str| json!({ "chat": "chat-alpha-1", "answers": [{ "question_id": question, "labels": [label] }] });
        // Auto-accept edits may approve an edit, never a command, and may
        // always decline.
        let world = approval_world("auto-accept-edits");
        let tools = self::tools(world.clone(), origin.clone());
        let err = tools
            .call("respond_to_input", answer("q-1", "Approve"))
            .await
            .unwrap_err();
        assert!(err.contains("Auto-accept edits"), "{err}");
        tools
            .call("respond_to_input", answer("q-2", "approve"))
            .await
            .unwrap();
        tools
            .call("respond_to_input", answer("q-1", "Decline"))
            .await
            .unwrap();
        let err = tools
            .call("respond_to_input", answer("q-1", "Sure thing"))
            .await
            .unwrap_err();
        assert!(err.contains("not a decision"), "{err}");
        {
            let writes = world.writes.lock().unwrap();
            assert_eq!(writes.len(), 2, "refused answers never reach the ledger");
            assert_eq!(writes[0].1["command"]["kind"], "respondInput");
            assert_eq!(writes[0].1["command"]["requestId"], "req-1");
        }

        // Full access may approve anything for the session.
        let world = approval_world("full-access");
        let tools = self::tools(world.clone(), origin);
        tools
            .call(
                "respond_to_input",
                answer("q-1", "Approve for this session"),
            )
            .await
            .unwrap();
        assert_eq!(world.writes.lock().unwrap().len(), 1);

        // Outside a chat nothing grants an approval.
        let tools = self::tools(approval_world("full-access"), Origin::default());
        let err = tools
            .call("respond_to_input", answer("q-1", "Approve"))
            .await
            .unwrap_err();
        assert!(err.contains("no Godspeed chat"), "{err}");
    }

    #[tokio::test]
    async fn conductor_agents_approve_low_risk_only_and_never_answer_questions() {
        let origin = Origin {
            chat_id: Some("chat-boss-3".into()),
            device_id: Some("dev-local".into()),
        };
        let answer = |question: &str, label: &str| json!({ "chat": "chat-alpha-1", "answers": [{ "question_id": question, "labels": [label] }] });
        let world = approval_world_in("full-access", vec![("chat-boss-3", 0)]);
        let tools = self::tools(world.clone(), origin.clone());
        // Medium risk: refused with the risk named, even in full access.
        let err = tools
            .call("respond_to_input", answer("q-1", "Approve"))
            .await
            .unwrap_err();
        assert!(err.contains("medium risk"), "{err}");
        assert!(err.contains("low-risk"), "{err}");
        // Low risk goes through, and declining is always allowed.
        tools
            .call("respond_to_input", answer("q-3", "Approve"))
            .await
            .unwrap();
        tools
            .call("respond_to_input", answer("q-1", "Decline"))
            .await
            .unwrap();
        assert_eq!(world.writes.lock().unwrap().len(), 2);

        // The same agent outside a conductor keeps the access-mode rule only.
        let plain = self::tools(approval_world("full-access"), origin);
        plain
            .call("respond_to_input", answer("q-1", "Approve"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn the_conductor_rule_follows_the_answered_chat_for_any_agent() {
        // The boss is outside every conductor and in full access; the chat it
        // answers is a conductor sub-agent, so the conductor rule applies.
        let origin = Origin {
            chat_id: Some("chat-boss-3".into()),
            device_id: Some("dev-local".into()),
        };
        let answer = |question: &str, label: &str| json!({ "chat": "chat-alpha-1", "answers": [{ "question_id": question, "labels": [label] }] });
        let world = approval_world_in("full-access", vec![("chat-alpha-1", 1)]);
        let tools = self::tools(world.clone(), origin.clone());
        let err = tools
            .call("respond_to_input", answer("q-1", "Approve"))
            .await
            .unwrap_err();
        assert!(err.contains("medium risk"), "{err}");
        let err = tools
            .call("respond_to_input", answer("q-4", "Postgres"))
            .await
            .unwrap_err();
        assert!(err.contains("wait for the user"), "{err}");
        // Low risk, approved once, and declines go through.
        tools
            .call("respond_to_input", answer("q-3", "Approve"))
            .await
            .unwrap();
        tools
            .call("respond_to_input", answer("q-1", "Decline"))
            .await
            .unwrap();
        assert_eq!(world.writes.lock().unwrap().len(), 2);

        // Outside a conductor on both ends, the same agent answers freely.
        let plain = approval_world_in("full-access", Vec::new());
        let tools = self::tools(plain.clone(), origin);
        tools
            .call("respond_to_input", answer("q-4", "Postgres"))
            .await
            .unwrap();
        tools
            .call("respond_to_input", answer("q-3", "Always approve"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn conductor_agents_approve_once_never_for_the_session_or_always() {
        let origin = Origin {
            chat_id: Some("chat-boss-3".into()),
            device_id: Some("dev-local".into()),
        };
        let answer = |label: &str| json!({ "chat": "chat-alpha-1", "answers": [{ "question_id": "q-3", "labels": [label] }] });
        let world = approval_world_in("full-access", vec![("chat-boss-3", 0), ("chat-alpha-1", 1)]);
        let tools = self::tools(world.clone(), origin);
        for label in ["Approve for this session", "Always approve"] {
            let err = tools
                .call("respond_to_input", answer(label))
                .await
                .unwrap_err();
            assert!(err.contains("one request at a time"), "{err}");
        }
        assert!(world.writes.lock().unwrap().is_empty());
        tools
            .call("respond_to_input", answer("Approve"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_failed_conductor_lookup_refuses_approvals() {
        let origin = Origin {
            chat_id: Some("chat-boss-3".into()),
            device_id: Some("dev-local".into()),
        };
        let answer = |question: &str, label: &str| json!({ "chat": "chat-alpha-1", "answers": [{ "question_id": question, "labels": [label] }] });
        let world = Arc::new(World {
            membership_error: true,
            ..approval_world_parts("full-access", Vec::new())
        });
        let tools = self::tools(world.clone(), origin);
        let err = tools
            .call("respond_to_input", answer("q-3", "Approve"))
            .await
            .unwrap_err();
        assert!(err.contains("Couldn't check"), "{err}");
        let err = tools
            .call("respond_to_input", answer("q-4", "Postgres"))
            .await
            .unwrap_err();
        assert!(err.contains("Couldn't check"), "{err}");
        assert!(world.writes.lock().unwrap().is_empty());
        // Declining grants nothing, so it still goes through.
        tools
            .call("respond_to_input", answer("q-1", "Decline"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn conductor_agents_cannot_leave_the_tree() {
        let main = Origin {
            chat_id: Some("chat-boss-3".into()),
            device_id: None,
        };
        let world = approval_world_in("full-access", vec![("chat-boss-3", 0)]);
        let tools = self::tools(world.clone(), main);
        let err = tools
            .call(
                "create_chat",
                json!({ "kind": "chat", "project": "space-1" }),
            )
            .await
            .unwrap_err();
        assert!(err.contains("standalone"), "{err}");
        let err = tools
            .call(
                "create_chats",
                json!({ "requests": [{ "kind": "chat", "project": "space-1" }] }),
            )
            .await
            .map(|v| v.to_string())
            .unwrap_or_else(|e| e);
        assert!(err.contains("standalone"), "{err}");
        // A parent outside the conductor is refused too.
        let err = tools
            .call(
                "create_chat",
                json!({ "parent": "chat-beta-2", "project": "space-1" }),
            )
            .await
            .unwrap_err();
        assert!(err.contains("outside it"), "{err}");
        assert!(world.writes.lock().unwrap().is_empty());
        // A sub-agent inside the conductor is fine.
        tools
            .call("create_chat", json!({ "project": "space-1" }))
            .await
            .unwrap();

        // A depth-2 member is refused standalone chats as well.
        let world = Arc::new(World {
            beta_parent: Some("chat-alpha-1".into()),
            conductor: vec![("chat-beta-2", 2)],
            ..Default::default()
        });
        let deep = Origin {
            chat_id: Some("chat-beta-2".into()),
            device_id: None,
        };
        let err = self::tools(world.clone(), deep)
            .call(
                "create_chat",
                json!({ "kind": "chat", "project": "space-1" }),
            )
            .await
            .unwrap_err();
        assert!(err.contains("standalone"), "{err}");
        assert!(world.writes.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn preview_tools_drive_only_the_calling_chat() {
        let world = Arc::new(World::default());
        let outside = tools(world.clone(), Origin::default());
        assert!(!outside.has("preview_open"));
        assert!(
            !outside
                .list()
                .await
                .iter()
                .any(|t| t.name.starts_with("preview_"))
        );
        let inside = tools(
            world.clone(),
            Origin {
                chat_id: Some("chat-alpha-1".into()),
                device_id: Some("dev-local".into()),
            },
        );
        for name in crate::preview::TOOLS {
            assert!(inside.has(name), "{name}");
        }
        // Validation happens before the engine is called.
        let err = inside
            .call("preview_open", json!({ "url": "file:///etc/passwd" }))
            .await
            .unwrap_err();
        assert!(err.contains("blocked"), "{err}");
        assert!(world.writes.lock().unwrap().is_empty());

        inside
            .call("preview_open", json!({ "url": "localhost:3000" }))
            .await
            .unwrap();
        let (method, params) = world.writes.lock().unwrap().last().unwrap().clone();
        assert_eq!(method, methods::PREVIEW_ACT);
        assert_eq!(params["chatId"], "chat-alpha-1");
        assert_eq!(params["action"], json!({ "action": "open", "url": "localhost:3000" }));

        let snapshot = inside.call("preview_snapshot", json!({})).await.unwrap();
        assert!(snapshot.as_str().unwrap().contains("[ref=e1]"));

        // The engine's error reaches the agent as the tool's error.
        let err = inside
            .call("preview_click", json!({ "ref": "e1" }))
            .await
            .unwrap_err();
        assert_eq!(err, zeron_proto::preview_agent::PREVIEW_NEEDS_WINDOW);

        // A screenshot comes back as an MCP image block plus text.
        let reply = crate::jsonrpc::handle_request(
            &inside,
            json!(7),
            "tools/call",
            json!({ "name": "preview_screenshot", "arguments": {} }),
        )
        .await;
        let content = reply["result"]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "image");
        assert_eq!(content[0]["mimeType"], "image/png");
        assert_eq!(content[0]["data"], "iVBORw0KGgo=");
        assert_eq!(content[1]["type"], "text");
        assert!(reply["result"]["structuredContent"].get("$image").is_none());
        assert_eq!(reply["result"]["structuredContent"]["width"], 640);
    }

    #[tokio::test]
    async fn conductor_tools_are_listed_only_for_members() {
        let world = Arc::new(World {
            conductor: vec![("chat-boss-3", 0)],
            ..Default::default()
        });
        let names = |tools: Vec<ToolDef>| tools.into_iter().map(|t| t.name).collect::<Vec<_>>();
        let member = tools(
            world.clone(),
            Origin {
                chat_id: Some("chat-boss-3".into()),
                device_id: None,
            },
        );
        assert!(names(member.list().await).contains(&"conductor_info"));
        let plain = tools(
            world,
            Origin {
                chat_id: Some("chat-beta-2".into()),
                device_id: None,
            },
        );
        let listed = names(plain.list().await);
        assert!(listed.contains(&"show_visual"));
        assert!(!listed.iter().any(|name| is_conductor_tool(name)));
    }

    #[tokio::test]
    async fn conductor_members_nest_to_the_cap_and_plain_side_chats_still_cannot() {
        let member = |id: &str| Origin {
            chat_id: Some(id.into()),
            device_id: None,
        };
        let spawn = json!({ "parent": "chat-beta-2", "project": "space-1" });
        // Depth 2 may start a sub-agent.
        let world = Arc::new(World {
            beta_parent: Some("chat-alpha-1".into()),
            conductor: vec![("chat-beta-2", 2)],
            ..Default::default()
        });
        tools(world.clone(), member("chat-beta-2"))
            .call("create_chat", spawn.clone())
            .await
            .unwrap();
        assert!(
            world
                .writes
                .lock()
                .unwrap()
                .iter()
                .any(|(_, w)| w["op"] == "createChat" && w["parentChatId"] == "chat-beta-2")
        );
        // Depth 3 is at the cap: refused, naming it.
        let world = Arc::new(World {
            beta_parent: Some("chat-alpha-1".into()),
            conductor: vec![("chat-beta-2", 3)],
            ..Default::default()
        });
        let err = tools(world.clone(), member("chat-beta-2"))
            .call("create_chat", spawn)
            .await
            .unwrap_err();
        assert!(err.contains("3 levels"), "{err}");
        assert!(world.writes.lock().unwrap().is_empty());
        // Outside a conductor the old rule holds.
        let world = Arc::new(World {
            beta_parent: Some("chat-alpha-1".into()),
            ..Default::default()
        });
        let err = tools(world, member("chat-beta-2"))
            .call("create_chat", json!({}))
            .await
            .unwrap_err();
        assert!(err.contains("Side chats cannot"), "{err}");
    }

    #[tokio::test]
    async fn conductor_tools_report_the_tree_and_keep_notes() {
        let world = Arc::new(World {
            conductor: vec![("chat-boss-3", 0), ("chat-alpha-1", 1)],
            extra_chats: vec![json!({
                "id": "chat-boss-3", "deviceId": "dev-local", "title": "Boss",
                "archived": false, "createdAt": "2026-09-03T00:00:00Z"
            })],
            ..Default::default()
        });
        let main = Origin {
            chat_id: Some("chat-boss-3".into()),
            device_id: None,
        };
        let t = tools(world.clone(), main);
        let info = t.call("conductor_info", json!({})).await.unwrap();
        assert_eq!(info["conductor"]["name"], "Acme");
        assert_eq!(info["repos"][0]["path"], "/repo/comet");
        assert_eq!(info["subAgents"][0]["chatId"], "chat-alpha-1");
        assert_eq!(info["subAgents"][0]["depth"], 1);
        assert_eq!(info["mayStartSubAgents"], true);
        t.call("conductor_notes_write", json!({ "notes": "ship fridays" }))
            .await
            .unwrap();
        let notes = t.call("conductor_notes_read", json!({})).await.unwrap();
        assert_eq!(notes["notes"], "ship fridays");

        // A chat outside every conductor gets a clear refusal, and the tools
        // are not offered outside a chat at all.
        let plain = tools(
            world.clone(),
            Origin {
                chat_id: Some("chat-beta-2".into()),
                device_id: None,
            },
        );
        let err = plain.call("conductor_info", json!({})).await.unwrap_err();
        assert!(err.contains("doesn't belong to a conductor"), "{err}");
        assert!(!tools(world, Origin::default()).has("conductor_info"));
    }

    #[tokio::test]
    async fn delegated_chats_never_exceed_the_origin_access_mode() {
        let origin = Origin {
            chat_id: Some("chat-boss-3".into()),
            device_id: Some("dev-local".into()),
        };
        let world = approval_world("auto-accept-edits");
        let tools = self::tools(world.clone(), origin);
        tools
            .call(
                "create_chat",
                json!({ "kind": "chat", "project": "space-1", "access_mode": "full-access" }),
            )
            .await
            .unwrap();
        tools
            .call(
                "create_chat",
                json!({ "kind": "chat", "project": "space-1" }),
            )
            .await
            .unwrap();
        tools
            .call(
                "create_chat",
                json!({ "kind": "chat", "project": "space-1", "sandbox": "read-only" }),
            )
            .await
            .unwrap();
        let writes = world.writes.lock().unwrap();
        let modes: Vec<&Value> = writes
            .iter()
            .filter(|(_, w)| w["op"] == "createChat")
            .map(|(_, w)| &w["config"]["accessMode"])
            .collect();
        assert_eq!(
            modes,
            [
                &json!("auto-accept-edits"),
                &json!("auto-accept-edits"),
                &json!("supervised")
            ]
        );
    }

    #[tokio::test]
    async fn create_chat_writes_the_row_and_validates_model() {
        let world = Arc::new(World::default());
        let tools = tools(world.clone(), Origin::default());
        let err = tools
            .call(
                "create_chat",
                json!({ "project": "space-1", "model": "nope" }),
            )
            .await
            .unwrap_err();
        assert!(err.contains("not offered"), "{err}");

        let created = tools
            .call(
                "create_chat",
                json!({ "project": "space-1", "model": "sonnet", "title": "Review", "prompt": "go" }),
            )
            .await
            .unwrap();
        let chat_id = created["chatId"].as_str().unwrap().to_owned();
        let writes = world.writes.lock().unwrap();
        assert_eq!(writes.len(), 3, "createChat, renameChat, run");
        assert_eq!(writes[0].1["op"], "createChat");
        assert_eq!(writes[0].1["chatId"], chat_id);
        assert_eq!(writes[0].1["spaceId"], "space-1");
        assert!(
            writes[0].1.get("parentChatId").is_none(),
            "no origin, no parent"
        );
        assert_eq!(writes[0].1["config"]["harness"], "claude-code");
        assert_eq!(writes[0].1["config"]["model"], "sonnet");
        assert_eq!(writes[1].1["op"], "renameChat");
        assert_eq!(writes[2].0, methods::QUEUE_COMMAND);
        assert_eq!(writes[2].1["command"]["request"]["prompt"], "go");
    }

    #[tokio::test]
    async fn create_chat_records_the_origin_as_parent() {
        let world = Arc::new(World::default());
        let tools = tools(
            world.clone(),
            Origin {
                chat_id: Some("chat-beta-2".into()),
                device_id: None,
            },
        );
        let created = tools
            .call("create_chat", json!({ "project": "space-1" }))
            .await
            .unwrap();
        assert_eq!(created["parentChatId"], "chat-beta-2");
        assert_eq!(
            world.writes.lock().unwrap()[0].1["parentChatId"],
            "chat-beta-2"
        );

        // An explicit parent (by title) overrides the origin.
        let created = tools
            .call(
                "create_chat",
                json!({ "project": "space-1", "parent": "Alpha" }),
            )
            .await
            .unwrap();
        assert_eq!(created["parentChatId"], "chat-alpha-1");
        let err = tools
            .call(
                "create_chat",
                json!({ "project": "space-1", "parent": "nope" }),
            )
            .await
            .unwrap_err();
        assert!(err.contains("no chat matches"), "{err}");
    }

    #[tokio::test]
    async fn wait_on_an_idle_chat_returns_immediately() {
        let world = Arc::new(World::default());
        let tools = tools(world, Origin::default());
        let waited = tools
            .call(
                "wait_for_turn",
                json!({ "chat": "alpha", "timeout_secs": 5 }),
            )
            .await
            .unwrap();
        assert_eq!(waited["turn"]["outcome"], "completed");
        assert_eq!(waited["turn"]["replies"][0]["text"], "hello back");
    }

    #[tokio::test]
    async fn send_with_wait_keeps_waiting_until_a_turn_lands() {
        // The stub never grows a session row, so a send-then-wait must run
        // to its deadline rather than declaring the (unstarted) run done.
        let world = Arc::new(World::default());
        let tools = tools(world, Origin::default());
        let started = std::time::Instant::now();
        let sent = tools
            .call(
                "send_message",
                json!({ "chat": "alpha", "text": "hi", "wait": true, "timeout_secs": 1 }),
            )
            .await
            .unwrap();
        assert_eq!(sent["turn"]["outcome"], "timedOut");
        assert!(started.elapsed() >= Duration::from_millis(900));
    }

    #[tokio::test]
    async fn batches_dispatch_all_chats_before_waiting_and_keep_partial_results() {
        for (name, requests) in [
            (
                "create_chats",
                json!([
                    {"prompt": "first", "wait": true, "timeout_secs": 1},
                    {"harness": "not-a-harness"},
                    {"prompt": "second", "wait": true, "timeout_secs": 1},
                ]),
            ),
            (
                "send_messages",
                json!([
                    {"chat": "alpha", "text": "first", "wait": true, "timeout_secs": 1},
                    {"chat": "missing", "text": "invalid"},
                    {"chat": "beta", "text": "second", "wait": true, "timeout_secs": 1},
                ]),
            ),
        ] {
            let world = Arc::new(World {
                dispatch_barrier: Some(tokio::sync::Barrier::new(2)),
                ..Default::default()
            });
            let tools = tools(world.clone(), Origin::default());
            let reply = tokio::time::timeout(
                Duration::from_secs(3),
                crate::jsonrpc::handle_request(
                    &tools,
                    json!(42),
                    "tools/call",
                    json!({"name": name, "arguments": {"requests": requests}}),
                ),
            )
            .await
            .expect("both dispatches must proceed while other requests are waiting");
            assert_eq!(reply["result"]["isError"], false, "{reply}");
            let results = &reply["result"]["structuredContent"]["results"];
            for i in [0, 2] {
                assert_eq!(results[i]["index"], i);
                assert_eq!(results[i]["isError"], false, "{reply}");
                assert_eq!(results[i]["result"]["turn"]["outcome"], "timedOut");
            }
            assert_eq!(results[1]["index"], 1);
            assert_eq!(results[1]["isError"], true);
            let writes = world.writes.lock().unwrap();
            let dispatched: Vec<_> = writes
                .iter()
                .filter(|(method, _)| method == methods::QUEUE_COMMAND)
                .collect();
            assert_eq!(dispatched.len(), 2);
            assert_ne!(dispatched[0].1["chatId"], dispatched[1].1["chatId"]);
        }
    }

    #[tokio::test]
    async fn invalid_batch_sizes_have_no_side_effects() {
        let world = Arc::new(World::default());
        let tools = tools(world.clone(), Origin::default());
        for name in ["create_chats", "send_messages"] {
            for requests in [vec![], vec![json!({"prompt": "hello"}); MAX_BATCH + 1]] {
                assert!(
                    tools
                        .call(name, json!({"requests": requests}))
                        .await
                        .is_err()
                );
            }
        }
        assert!(world.writes.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn side_chats_cannot_create_chats_or_be_parents() {
        let world = Arc::new(World {
            beta_parent: Some("chat-alpha-1".into()),
            ..Default::default()
        });
        let side = tools(
            world.clone(),
            Origin {
                chat_id: Some("chat-beta-2".into()),
                device_id: None,
            },
        );
        for args in [json!({}), json!({"parent":"Alpha"}), json!({"kind":"chat"})] {
            assert!(
                side.call("create_chat", args)
                    .await
                    .unwrap_err()
                    .contains("Side chats cannot")
            );
        }
        let batch = side
            .call(
                "create_chats",
                json!({"requests":[{}, {"parent":"Alpha"}, {"kind":"chat"}]}),
            )
            .await
            .unwrap();
        assert!(
            batch["results"]
                .as_array()
                .unwrap()
                .iter()
                .all(|r| r["isError"] == true)
        );
        let root = tools(world.clone(), Origin::default());
        assert!(
            root.call("create_chat", json!({"parent":"Beta"}))
                .await
                .unwrap_err()
                .contains("child of a side chat")
        );
        assert!(world.writes.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn kinds_preserve_defaults_and_reject_invalid_arguments_before_writing() {
        let world = Arc::new(World::default());
        let root = tools(world.clone(), Origin::default());
        for args in [
            json!({"kind":"side"}),
            json!({"kind":"unknown"}),
            json!({"kind":"chat", "parent":"Alpha", "title":"must not write"}),
        ] {
            assert!(root.call("create_chat", args).await.is_err());
        }
        assert!(world.writes.lock().unwrap().is_empty());
        let from_chat = tools(
            world.clone(),
            Origin {
                chat_id: Some("chat-alpha-1".into()),
                device_id: None,
            },
        );
        for (args, kind, parent) in [
            (json!({"kind":"chat"}), "chat", Value::Null),
            (json!({"kind":"chat", "parent":"  "}), "chat", Value::Null),
            (json!({"kind":"side"}), "side", json!("chat-alpha-1")),
            (json!({}), "side", json!("chat-alpha-1")),
            (json!({"parent":"Beta"}), "side", json!("chat-beta-2")),
        ] {
            let created = from_chat.call("create_chat", args).await.unwrap();
            assert_eq!(created["kind"], kind);
            assert_eq!(created["parentChatId"], parent);
            let writes = world.writes.lock().unwrap();
            let row = &writes.last().unwrap().1;
            if kind == "chat" {
                assert!(row.get("parentChatId").is_none());
            } else {
                assert_eq!(row["parentChatId"], parent);
            }
        }
        assert_eq!(
            root.call("create_chat", json!({})).await.unwrap()["kind"],
            "chat"
        );
        assert_eq!(
            root.call("create_chat", json!({"kind":"side", "parent":"Alpha"}))
                .await
                .unwrap()["kind"],
            "side"
        );
    }

    #[tokio::test]
    async fn project_and_device_resolution_scopes_names_and_exact_paths() {
        let world = Arc::new(World::default());
        let tools = tools(world.clone(), Origin::default());
        for args in [
            json!({"project":"comet"}),
            json!({"project":"/repo/comet"}),
            json!({"project":"space-1", "device":"Worker"}),
            json!({"project":"missing", "device":"Worker"}),
            json!({"device":"missing"}),
        ] {
            let error = tools.call("create_chat", args).await.unwrap_err();
            assert!(world.writes.lock().unwrap().is_empty(), "{error}");
        }
        let error = tools
            .call("create_chat", json!({"project":"/repo/comet"}))
            .await
            .unwrap_err();
        for candidate in ["space-1", "space-remote", "dev-local", "dev-remote"] {
            assert!(error.contains(candidate), "{error}");
        }
        for project in ["comet", "/repo/comet", "space-remote"] {
            let created = tools
                .call("create_chat", json!({"project":project,"device":"Worker"}))
                .await
                .unwrap();
            assert_eq!(created["deviceId"], "dev-remote");
            assert_eq!(created["project"]["id"], "space-remote");
            assert_eq!(created["harness"], "codex");
        }
        let created = tools
            .call("create_chat", json!({"project":"unique"}))
            .await
            .unwrap();
        assert_eq!(created["deviceId"], "dev-remote");
        let created = tools
            .call("create_chat", json!({"device":"dev-remote"}))
            .await
            .unwrap();
        assert!(created["project"].is_null());
        assert_eq!(created["deviceId"], "dev-remote");
        let created = tools.call("create_chat", json!({})).await.unwrap();
        assert_eq!(created["deviceId"], "dev-local");
    }

    #[tokio::test]
    async fn discovery_and_creation_use_the_selected_hosts_catalogs() {
        let world = Arc::new(World::default());
        let tools = tools(world.clone(), Origin::default());
        assert_eq!(
            tools.call("list_projects", json!({})).await.unwrap()["projects"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
        let projects = tools
            .call("list_projects", json!({"device":"Worker"}))
            .await
            .unwrap();
        assert_eq!(projects["projects"].as_array().unwrap().len(), 2);
        assert!(
            projects["projects"]
                .as_array()
                .unwrap()
                .iter()
                .all(|p| p["deviceId"] == "dev-remote")
        );
        assert_eq!(
            tools.call("list_harnesses", json!({})).await.unwrap()["harnesses"][0]["id"],
            "claude-code"
        );
        assert_eq!(
            tools
                .call("list_harnesses", json!({"device":"Worker"}))
                .await
                .unwrap()["harnesses"][0]["id"],
            "codex"
        );
        assert_eq!(
            tools
                .call("list_models", json!({"harness":"codex", "device":"Worker"}))
                .await
                .unwrap()["models"][0]["id"],
            "remote-model"
        );
        assert_eq!(
            tools
                .call("list_models", json!({"harness":"claude-code"}))
                .await
                .unwrap()["models"][0]["id"],
            "opus"
        );
        for args in [
            json!({"device":"Worker", "harness":"claude-code"}),
            json!({"device":"Worker", "harness":"cursor"}),
            json!({"device":"Laptop", "harness":"codex"}),
            json!({"device":"Laptop", "harness":"mock"}),
            json!({"device":"Worker", "harness":"codex", "model":"opus"}),
        ] {
            assert!(tools.call("create_chat", args).await.is_err());
            assert!(world.writes.lock().unwrap().is_empty());
        }
        let created = tools
            .call(
                "create_chat",
                json!({"device":"Worker", "harness":"codex", "model":"remote-model"}),
            )
            .await
            .unwrap();
        assert_eq!(created["harness"], "codex");
        assert_eq!(created["model"], "remote-model");
        for method in [methods::LIST_HARNESSES, methods::LIST_MODELS] {
            let broken = Arc::new(World {
                catalog_error: Some(method),
                ..Default::default()
            });
            let tools = self::tools(broken.clone(), Origin::default());
            let error = tools
                .call(
                    "create_chat",
                    json!({"device":"Worker", "model":"remote-model"}),
                )
                .await
                .unwrap_err();
            assert!(
                error.contains("dev-remote") && error.contains("catalog unavailable"),
                "{error}"
            );
            assert!(broken.writes.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn remote_send_selects_the_remote_default_harness() {
        let world = Arc::new(World {
            beta_remote: true,
            ..Default::default()
        });
        let tools = tools(world.clone(), Origin::default());
        tools
            .call("send_message", json!({"chat":"Beta", "text":"go"}))
            .await
            .unwrap();
        assert_eq!(
            world.reads.lock().unwrap().last().unwrap().1["targetDeviceId"],
            "dev-remote"
        );
        assert_eq!(
            world.writes.lock().unwrap().last().unwrap().1["command"]["request"]["harness"],
            "codex"
        );
    }

    #[tokio::test]
    async fn mixed_batch_preserves_order_and_errors_per_request() {
        let world = Arc::new(World::default());
        let tools = tools(
            world.clone(),
            Origin {
                chat_id: Some("chat-alpha-1".into()),
                device_id: None,
            },
        );
        let result = tools
            .call(
                "create_chats",
                json!({"requests":[
                    {"kind":"chat", "device":"Worker", "project":"comet"},
                    {"kind":"chat", "parent":"Alpha"},
                    {"kind":"side", "project":"space-1"},
                    {"kind":"bogus"}
                ]}),
            )
            .await
            .unwrap();
        let results = &result["results"];
        assert_eq!(results[0]["result"]["kind"], "chat");
        assert_eq!(results[0]["result"]["deviceId"], "dev-remote");
        assert_eq!(results[1]["isError"], true);
        assert_eq!(results[2]["result"]["kind"], "side");
        assert_eq!(results[2]["result"]["parentChatId"], "chat-alpha-1");
        assert_eq!(results[3]["isError"], true);
        for i in 0..4 {
            assert_eq!(results[i]["index"], i);
        }
        assert_eq!(world.writes.lock().unwrap().len(), 2);
    }

    /// Sessions and transcripts are independently delivered. The command reply
    /// is deliberately slow, so the assistant starts before QueueCommand returns.
    struct DelayedTurn {
        world: World,
        sent: Mutex<Option<(i64, Instant, String)>>,
        reply_delay: Duration,
        never_reply: bool,
        /// Host clock offset: a remote engine stamps createdAt on its own clock.
        host_skew_millis: i64,
    }

    #[async_trait]
    impl RpcService for DelayedTurn {
        async fn handle(&self, method: &str, params: Value) -> Result<RpcReply, RpcError> {
            match method {
                methods::WATCH_CHATS => {
                    let reply = self.world.handle(method, params).await?;
                    let RpcReply::Stream(mut rows) = reply else {
                        unreachable!()
                    };
                    let mut rows = rows.next().await.unwrap();
                    if let Some((_, _, chat)) = self.sent.lock().unwrap().as_ref()
                        && chat != "chat-beta-2"
                    {
                        rows.as_array_mut().unwrap().push(json!({
                            "id": chat, "deviceId":"dev-remote", "archived":false,
                            "createdAt":chrono::Utc::now(), "config":{"harness":"codex", "sandbox":"workspace-write"}
                        }));
                    }
                    Ok(stream(rows))
                }
                methods::QUEUE_COMMAND => {
                    let reply = self.world.handle(method, params.clone()).await?;
                    *self.sent.lock().unwrap() = Some((
                        now_millis(),
                        Instant::now(),
                        params["chatId"].as_str().unwrap().into(),
                    ));
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    Ok(reply)
                }
                methods::WATCH_SESSIONS => {
                    let sent = self.sent.lock().unwrap().clone();
                    let Some((_, _, chat)) = sent else {
                        return Ok(stream(json!([])));
                    };
                    Ok(RpcReply::Stream(
                        futures::stream::once(async { json!([]) })
                            .chain(futures::stream::once(async move {
                                tokio::time::sleep(Duration::from_millis(50)).await;
                                json!([{"chatId":chat, "deviceId":"dev-remote", "status":"idle",
                                "updatedAt":chrono::Utc::now(), "lastCompletedTurn":"new-turn"}])
                            }))
                            .boxed(),
                    ))
                }
                methods::WATCH_DOC_MESSAGES => {
                    let sent = self.sent.lock().unwrap().clone();
                    // Only the existing chat has history. Its previous reply
                    // shares the send's timestamp, so only its id tells them apart.
                    let mut messages = json!([]);
                    if params["chatId"] == "chat-beta-2" {
                        let created_at = sent.as_ref().map_or_else(now_millis, |(t, _, _)| *t);
                        messages.as_array_mut().unwrap().push(json!({
                            "id":"previous-reply", "role":"assistant", "createdAt":created_at,
                            "deviceId":"dev-remote", "status":"complete", "parts":[{"kind":"text", "id":"t", "text":"old reply"}]
                        }));
                    }
                    if let Some((timestamp, started, _)) = sent
                        && started.elapsed() >= self.reply_delay
                        && !self.never_reply
                    {
                        messages.as_array_mut().unwrap().push(json!({
                            "id":"new-reply", "role":"assistant", "createdAt":timestamp + self.host_skew_millis,
                            "deviceId":"dev-remote", "status":"complete", "parts":[{"kind":"text", "id":"t", "text":"new reply"}]
                        }));
                    }
                    Ok(stream(json!({"reset":messages})))
                }
                _ => self.world.handle(method, params).await,
            }
        }
    }

    #[tokio::test]
    async fn wait_allows_a_missing_session_and_a_late_remote_transcript() {
        for create in [true, false] {
            let service = Arc::new(DelayedTurn {
                world: World {
                    beta_remote: true,
                    ..Default::default()
                },
                sent: Mutex::new(None),
                reply_delay: Duration::from_millis(450),
                never_reply: false,
                host_skew_millis: 0,
            });
            let tools = Tools::new(Arc::new(Godspeed::with_client(
                memory_client(service),
                Origin::default(),
            )));
            let (name, args) = if create {
                (
                    "create_chat",
                    json!({"kind":"chat", "device":"Worker", "prompt":"go", "wait":true, "timeout_secs":2}),
                )
            } else {
                (
                    "send_message",
                    json!({"chat":"Beta", "text":"go", "wait":true, "timeout_secs":2}),
                )
            };
            let result = tools.call(name, args).await.unwrap();
            assert_eq!(result["turn"]["outcome"], "completed", "{result}");
            assert_eq!(
                result["turn"]["replies"].as_array().unwrap().len(),
                1,
                "{result}"
            );
            assert_eq!(result["turn"]["replies"][0]["text"], "new reply");
        }
    }

    #[tokio::test]
    async fn standalone_wait_after_nonblocking_send_keeps_the_send_baseline() {
        for create in [true, false] {
            let service = Arc::new(DelayedTurn {
                world: World {
                    beta_remote: true,
                    ..Default::default()
                },
                sent: Mutex::new(None),
                reply_delay: Duration::from_millis(450),
                never_reply: false,
                host_skew_millis: 0,
            });
            let tools = Tools::new(Arc::new(Godspeed::with_client(
                memory_client(service),
                Origin::default(),
            )));
            let sent = if create {
                tools
                    .call(
                        "create_chat",
                        json!({"kind":"chat", "device":"Worker", "prompt":"go", "wait":false}),
                    )
                    .await
                    .unwrap()
            } else {
                tools
                    .call(
                        "send_message",
                        json!({"chat":"Beta", "text":"go", "wait":false}),
                    )
                    .await
                    .unwrap()
            };
            let result = tools
                .call(
                    "wait_for_turn",
                    json!({"chat":sent["chatId"], "timeout_secs":2}),
                )
                .await
                .unwrap();
            assert_eq!(result["turn"]["outcome"], "completed", "{result}");
            assert_eq!(result["turn"]["replies"].as_array().unwrap().len(), 1);
            assert_eq!(result["turn"]["replies"][0]["text"], "new reply");
        }
    }

    /// A remote host stamps replies on its own clock. One running behind the
    /// caller must not hide the reply to this send.
    #[tokio::test]
    async fn wait_returns_the_reply_from_a_host_whose_clock_is_behind() {
        for create in [true, false] {
            let service = Arc::new(DelayedTurn {
                world: World {
                    beta_remote: true,
                    ..Default::default()
                },
                sent: Mutex::new(None),
                reply_delay: Duration::from_millis(100),
                never_reply: false,
                host_skew_millis: -30_000,
            });
            let tools = Tools::new(Arc::new(Godspeed::with_client(
                memory_client(service),
                Origin::default(),
            )));
            let (name, args) = if create {
                (
                    "create_chat",
                    json!({"kind":"chat", "device":"Worker", "prompt":"go", "wait":true, "timeout_secs":2}),
                )
            } else {
                (
                    "send_message",
                    json!({"chat":"Beta", "text":"go", "wait":true, "timeout_secs":2}),
                )
            };
            let result = tools.call(name, args).await.unwrap();
            assert_eq!(result["turn"]["outcome"], "completed", "{result}");
            let replies = result["turn"]["replies"].as_array().unwrap();
            assert!(replies.iter().any(|r| r["text"] == "new reply"), "{result}");
        }
    }

    #[tokio::test]
    async fn completed_session_without_new_transcript_times_out_without_old_reply() {
        let service = Arc::new(DelayedTurn {
            world: World {
                beta_remote: true,
                ..Default::default()
            },
            sent: Mutex::new(None),
            reply_delay: Duration::ZERO,
            never_reply: true,
            host_skew_millis: 0,
        });
        let tools = Tools::new(Arc::new(Godspeed::with_client(
            memory_client(service),
            Origin::default(),
        )));
        let result = tools
            .call(
                "send_message",
                json!({"chat":"Beta", "text":"go", "wait":true, "timeout_secs":1}),
            )
            .await
            .unwrap();
        assert_eq!(result["turn"]["outcome"], "timedOut");
        assert_eq!(result["turn"]["replies"], json!([]));
    }

    #[tokio::test]
    async fn remote_replies_do_not_require_synchronized_clocks() {
        for host_skew_millis in [-1_000, 1_000] {
            for wait in [true, false] {
                let service = Arc::new(DelayedTurn {
                    world: World {
                        beta_remote: true,
                        ..Default::default()
                    },
                    sent: Mutex::new(None),
                    reply_delay: Duration::from_millis(450),
                    never_reply: false,
                    host_skew_millis,
                });
                let tools = Tools::new(Arc::new(Godspeed::with_client(
                    memory_client(service),
                    Origin::default(),
                )));
                let sent = tools
                    .call(
                        "send_message",
                        json!({
                            "chat":"Beta", "text":"go", "wait":wait, "timeout_secs":2
                        }),
                    )
                    .await
                    .unwrap();
                let result = if wait {
                    sent
                } else {
                    tools
                        .call(
                            "wait_for_turn",
                            json!({
                                "chat":sent["chatId"], "timeout_secs":2
                            }),
                        )
                        .await
                        .unwrap()
                };
                assert_eq!(
                    result["turn"]["outcome"], "completed",
                    "offset={host_skew_millis}, wait={wait}: {result}"
                );
                assert_eq!(
                    result["turn"]["replies"].as_array().unwrap().len(),
                    1,
                    "{result}"
                );
                assert_eq!(result["turn"]["replies"][0]["id"], "new-reply");
            }
        }
    }

    #[tokio::test]
    async fn initialize_and_list_over_jsonrpc() {
        let world = Arc::new(World::default());
        let tools = tools(world, Origin::default());
        let init = crate::jsonrpc::handle_request(
            &tools,
            json!(1),
            "initialize",
            json!({ "protocolVersion": "2025-03-26" }),
        )
        .await;
        assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(init["result"]["serverInfo"]["name"], "godspeed");
        let list =
            crate::jsonrpc::handle_request(&tools, json!(2), "tools/list", Value::Null).await;
        assert!(list["result"]["tools"].as_array().unwrap().len() >= 10);
        let bad = crate::jsonrpc::handle_request(
            &tools,
            json!(3),
            "tools/call",
            json!({ "name": "nope" }),
        )
        .await;
        assert_eq!(bad["error"]["code"], -32602);
        let whoami = crate::jsonrpc::handle_request(
            &tools,
            json!(4),
            "tools/call",
            json!({ "name": "whoami" }),
        )
        .await;
        assert_eq!(whoami["result"]["isError"], false);
        assert_eq!(
            whoami["result"]["structuredContent"]["localDeviceId"],
            "dev-local"
        );
    }
}
