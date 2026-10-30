# Godspeed for iOS

A UIKit viewport onto your Godspeed machines, built on the Rust mobile core
(forked from Zeron's iOS app). The phone is a **peer device**: it mirrors the
workspace registry, joins per-chat session rooms, and drives remote engines
through the durable command ledger. No agent runs on the phone.

**Rust decides what to paint and where; Swift paints, scrolls and handles
gestures.** Everything in Rust is platform-neutral — the future Android app
links the same library. See [`docs/mobile-rewrite.md`](../../docs/mobile-rewrite.md).

| | |
| --- | --- |
| Bundle id | `site.shrithan.godspeed.ios` (`.LiveActivity`, `.tests`, `.uitests`) |
| Team | `QPUV967B5U` |
| URL scheme | `godspeed://` (`godspeed://callback` for sign-in, `godspeed://voice` from the Live Activity) |
| App icon | `Godspeed/Assets.xcassets/AppIcon.appiconset`, made from `dist/godspeed.png` by `scripts/ios/make-app-icon.py` |

## Build & run

Requires Xcode 26+ (built with Xcode 27) and a Rust toolchain with the iOS
targets:

```sh
rustup target add aarch64-apple-ios aarch64-apple-ios-sim
cd apps/ios

# Simulator. Use a named device: the Rust core is arm64-only, and the generic
# simulator destination also builds x86_64 (link fails). Create one with
#   xcrun simctl create "Godspeed iPhone 17 Pro" \
#     com.apple.CoreSimulator.SimDeviceType.iPhone-17-Pro com.apple.CoreSimulator.SimRuntime.iOS-27-0
xcodebuild -project Godspeed.xcodeproj -scheme Godspeed -configuration Debug \
  -destination 'platform=iOS Simulator,name=Godspeed iPhone 17 Pro' build

# Device (compile check without signing; drop CODE_SIGNING_ALLOWED to sign
# with the team above)
xcodebuild -project Godspeed.xcodeproj -scheme Godspeed -configuration Debug \
  -destination 'generic/platform=iOS' CODE_SIGNING_ALLOWED=NO build
```

The Godspeed target's **Rust core** build phase runs `scripts/ios/build-core.sh`,
which builds `crates/mobile` (crate `zeron-mobile`) for the active platform
(always optimized — the `mobile` cargo profile) into `<repo>/target`, and
refreshes the committed UniFFI bindings in `Godspeed/Core/Generated/`.
`GODSPEED_SKIP_CORE=1` reuses the last built library when iterating on Swift
only. The script runs cargo in a clean environment, so in a worktree point
`<repo>/target` at your build directory with a symlink and cap jobs in
`.cargo/config.toml` (`[build] jobs = 4`).

## Connecting to your edge

`Endpoints` in `Godspeed/Shell/SignInViewController.swift` points at the hosted edge
(`https://godspeed-edge.shrid5.workers.dev`) and its WorkOS AuthKit client, the same defaults as
the desktop (`crates/client/src/auth.rs`); `godspeed://callback` is the redirect URI. Cloud sync
there is invite-only (`edge/DEPLOY.md`, "The iPhone beta waitlist"). A self-hosted edge needs its
URL and client id in `Endpoints`, plus an APNs key for push (see Session notifications).

For development, run a local `wrangler dev` edge with `AUTH_MODE=dev` and launch a Debug build
with `-dev <userId> <orgId> -edge <url>` (see Launch arguments), or use `-demo` (the same offline
workspace as "Look around with sample data").

## Layout

```
Godspeed/
  App/         AppDelegate, SceneDelegate, AppModel (owns CoreClient; maps
               workspace snapshots to view models; Keychain credentials)
  Core/        Generated UniFFI bindings; Fonts (the exact bytes Rust measures,
               registered with CoreText) + CoreText fallback measurer
  Design/      Palette (theme color roles → light/dark), Glass helpers,
               cell-grid status glyph, harness brand marks
  Transcript/  TranscriptListView (virtualized scroll host over LayoutFrame),
               RowView/RowModel (CoreText painter for Rust display lists,
               streaming veil), FrameRelay
  Session/     SessionViewController, SessionSource (Core/Fixture), new-session
               canvas
  Composer/    ComposerBar (glass capsule ⇄ card with inline context chips;
               send/queue/steer/stop), question and queue panels, attachments
  Threads/     Sessions (foldable sidebar sections, Snoozed / Settled
               shelves)/folder/search lists, cells, new-project folder browser
  Inbox/       Needs You: every host's attention queue in risk order, swipe
               to approve / decline, "Approve all low-risk", tab + icon badge
  Board/       Mission Control: every agent by device and project
  Usage/       Settings → Usage (plan windows, history, cost at API prices)
               and the feed that writes the widgets' App Group snapshot
  Shell/       Tab bar (Sessions, Needs You, Board, Settings, search; "New
               session" accessory with live summary), sign-in
  Debug/       Transcript lab + hitch meter
GodspeedLiveActivity/  Widget extension: the voice call's Live Activity (Dynamic
               Island + Lock Screen), with the orb's states exported from
               zeron-orb as vector assets, and the usage widgets (small,
               medium, Lock Screen circular / rectangular / inline)
GodspeedShared/  Compiled into the app and the extension: the activity's
               attributes and its mute/end App Intents; usage formatting, the
               widget snapshot (App Group `group.site.shrithan.godspeed.ios`)
               and its timeline
```

### Transcript pipeline

1. `zeron-client` applies session-doc updates incrementally (O(changed
   entries)) and publishes immutable snapshots.
2. `TranscriptView.attach(client, chatId)` subscribes **in Rust**; the layout
   thread turns entries into rows (one per markdown block / user message / tool
   group), reparses streaming markdown incrementally, and measures every row
   with `zeron-text` at the viewport width.
3. Each pass publishes a `LayoutFrame`: exact heights + prefix-sum offsets.
   `rowsIn(y0, y1)` is a binary search; `display(i)` builds a display list
   (text runs at exact positions, boxes, links, scrollers, widgets).
4. `TranscriptListView` positions reusable `RowView`s at those offsets and
   paints runs with CoreText at the Rust coordinates — measurement and
   rendering can't disagree. Display models for rows beyond the viewport are
   prefetched off the main thread.
5. Agent visuals (`show_visual` Luau scripts) get a worker thread each
   (`crates/mobile/src/visual`): it runs the script, lays the node tree out,
   draws canvases and charts as vector paint, renders scenes and shaders with
   wgpu, and reports its height back to the row. `VisualView` paints what
   arrives (rasterized on the worker thread) and forwards touches; the demo's
   "Agent visuals" chat (`-demo -route chat:chat-visuals`) has one of each.

## Launch arguments

| Arg | Effect |
| --- | --- |
| `-demo` | Offline demo workspace (Rust `DemoHost`: registry, docs, streaming replies) |
| `-fast` / `-longreply` | Demo stream speed / reply length |
| `-big` / `-huge` | Demo transcripts with 120 / 600 turns |
| `-route chat:<id>` / `new` / `more` / `search` / `inbox` / `board` / `usage` | Open a screen at launch (also `godspeed://inbox`, `godspeed://board`, `godspeed://usage`) |
| `-signedout` | Clear stored credentials |
| `-dev <userId> <orgId> [-edge <url>]` | Dev bearer against an `AUTH_MODE=dev` edge (e.g. `wrangler dev`); not persisted |
| `-harness <id>` | Default harness for new sessions (`mock` for live-stack tests) |
| `-bench` (with `-lab`) | Display-link fling benchmark; writes `Documents/bench.json` |
| `-measureopen` | Session open latency; writes `Documents/open.json` |
| `-lab [-turns N] [-autostream] [-autoscroll] [-top] [-meter]` | Transcript lab over fixture markdown, with an on-screen hitch meter |

## Tests

```sh
# Rust
cargo test -p zeron-text -p zeron-markdown -p zeron-client -p zeron-mobile
cargo test --release -p zeron-mobile --lib bench_layout -- --ignored --nocapture

# Line-break accuracy vs CoreText (same font bytes), flows, hitch benchmarks
xcodebuild … -only-testing:GodspeedTests/LineBreakAccuracyTests test
xcodebuild … -only-testing:GodspeedUITests/SessionFlowTests test
xcodebuild … -only-testing:GodspeedUITests/ScrollPerformanceTests test
```

Live stack (real edge + headless engine; see `GodspeedUITests/LiveStackTests.swift`):

```sh
(cd edge && npx wrangler dev --port 27650 --var AUTH_MODE:dev) &
GODSPEED_DATA_DIR=/tmp/e2e GODSPEED_IPC_PORT=27811 GODSPEED_EDGE_URL=http://localhost:27650 \
  GODSPEED_EDGE_TOKEN=alice@org1 GODSPEED_ORG_ID=org1 GODSPEED_HARNESS=mock target/debug/godspeed headless &
TEST_RUNNER_GODSPEED_LIVE_EDGE=http://localhost:27650 xcodebuild … -only-testing:GodspeedUITests/LiveStackTests test
```

The test launches the app with `-harness mock`: a chat's configured harness
wins over the engine's `GODSPEED_HARNESS` default, so without it a real agent
would run.

## Needs You, Mission Control, Usage

The phone reaches each Mac's engine over the device-room relay (the same
path as `ListHarnesses`): the core keeps one `WatchAttention` stream per
execution host while foregrounded (`crates/client/src/attention.rs`), merges
them in the desktop's order, and answers with `RespondAttention` /
`ApproveLowRisk` / `DismissAttention` on the item's host. A host whose engine
predates the inbox (before 0.4) or whose stream is down contributes items
derived from its registry rows instead (open-only). Mission Control is built
from the synced registry rows plus the inbox. Usage reads `ListAgentAccounts`
(plan windows) and `GetUsageHistory` (0.5.1+) from the chosen host; the
widgets read the snapshot the app writes to the App Group. Thread lifecycle
(settle / snooze / wake) writes the chat's registry row; Limited choices go to
the chat's host with `SetChatLimitRecovery`. Turn receipts are built in the
layout engine from the doc's `turnStats` stamps
(`crates/mobile/src/layout/receipt.rs`, a port of the desktop's).

Push for the inbox is not wired yet. It needs the APNs key on the edge (see
below), an `approval` kind in `edge/src/push-notify.ts` that carries
`deviceId` + `itemId`, and a notification category with Approve / Decline
actions that call `respondAttention`; a tap routes through
`AppRouter.showInbox()` (`godspeed://inbox`).

## Session notifications

The edge sends them (`edge/src/push-notify.ts`, `registry-room.ts`): when a
session's registry row changes it applies the desktop's rule (run finished,
waiting on your input, run failed) and pushes to every phone that registered
with `POST /registry/:org/push-target`. The app asks for permission the first
time a session is started from the phone (or from Settings → Notifications),
registers its APNs token with the choices from Settings, stays quiet while
open, and opens the session a notification is tapped for.

Setup once per deployment: enable Push Notifications on the
`site.shrithan.godspeed.ios` App ID (Xcode's automatic signing does this when
the app is first signed for a device), create an APNs auth key (Apple
Developer → Keys → Apple Push Notifications service), then
`wrangler secret put APNS_KEY_P8` (the `.p8` contents) and
`wrangler secret put APNS_KEY_ID` in `edge/`. The team and topic are already
set (`APNS_TEAM_ID`, `APNS_TOPIC` in `edge/wrangler.jsonc`).
`/registry/:org/stats` shows registered phones and recent deliveries.

## TestFlight release

Public beta through TestFlight (step-by-step and tester approvals:
[`docs/godspeed/TESTFLIGHT.md`](../../docs/godspeed/TESTFLIGHT.md)).

```sh
scripts/ios/testflight.sh              # Rust core (mobile-dist) → Release archive → upload
scripts/ios/testflight.sh --no-upload  # same, but export the .ipa locally
scripts/ios/asc-setup.sh status        # app record, builds, review state, public link
scripts/ios/asc-setup.sh submit        # newest processed build → public group + beta review
```

- **Signing:** automatic, team `QPUV967B5U`, `-allowProvisioningUpdates` with the App Store
  Connect API key from `~/.config/outbound/keys.env` (`ASC_KEY_ID`, `ASC_ISSUER_ID`,
  `ASC_KEY_PATH`; never printed). Xcode creates the distribution profiles and the App Group.
- **Versions:** `MARKETING_VERSION` stays `1.0` in the project; the build number is a UTC
  timestamp (`YYYYMMDD.HHMM`) passed as `CURRENT_PROJECT_VERSION` at archive time, so every
  upload is higher than the last and the project file doesn't change.
- **App Store metadata** (test information, beta review notes, the public group) lives in
  `scripts/ios/asc.py`; the review contact and demo account come from
  `~/.config/godspeed/testflight.env` (`edge/scripts/review-account.mjs` creates the account).
- **Release readiness:** `ITSAppUsesNonExemptEncryption` is false (HTTPS/TLS via rustls and
  WebRTC's DTLS-SRTP only); `Godspeed/PrivacyInfo.xcprivacy` and
  `GodspeedLiveActivity/PrivacyInfo.xcprivacy` declare the collected data (matching
  `docs/PRIVACY.md`) and required-reason APIs; the icon is the single 1024 px
  `AppIcon1024.png` (Xcode renders the other sizes).
- **Reviewers and testers without a Mac:** "Look around with sample data" on the sign-in screen
  starts the offline demo workspace (the `-demo` fixtures); Settings → Leave Sample Data returns.
