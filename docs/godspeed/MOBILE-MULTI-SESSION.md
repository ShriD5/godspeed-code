# Multi-session chats on the iPhone

Owner report: "if it's a multi-session chat with multiple sessions, it's hard to see it on phone,
the experience for multi chat on phone is bad." This is the design for making a chat with
sub-sessions as readable on the phone as on the desktop. Built in `feat/mobile-multi-session`.

## What "multi-session" means here

| Kind | Data | Desktop | iPhone before this change |
| --- | --- | --- | --- |
| Conductor (main chat steering sub-agent chats) | child chats with `parentChatId` = main chat; nested up to depth 3 | conductor cards in the main transcript (status, steps, Open / Answer), sidebar entry with a needs-you flag | children invisible |
| Side chats (fork, "New side chat", MCP `kind: side`) | child chats with `parentChatId` | "Side chats" lane under the session card | children invisible |
| MCP `create_chats` from an agent | child chats of the calling chat unless `kind: chat` | same as side chats | children invisible |
| Claude Task-tool sub-agents inside one chat | `Agent` tool parts with `subagent_status`; their content lives in a separate doc | subagent lane, click opens the sub-agent | agent cards in the transcript (status glyph), not openable |
| Session lanes / session card / Mission Control | built from the above | lanes card; Mission Control shows top-level chats only | none |

The first three are the same thing on the wire: a chat row whose `parentChatId` points at another
chat. They're what the owner hits, and they're what this change fixes. Task-tool sub-agents
already render as cards inside the chat; opening one needs the sub-agent doc on the phone and is
left for later (see Not in this change).

## What was broken on the phone

Before/after screenshots are in PR #120 (demo mode: `-demo -route chat:chat-conductor`, a
conductor with four sub-agents and a grandchild, one waiting on an approval).

1. **Children are unreachable.** The front page, project lists, search and Mission Control filter
   out every chat with a parent (`crates/client/src/workspace/view.rs`, `BoardModel.build`).
   The core exposed `childSessions(parentId:)`, but no Swift code called it. The only way into a
   sub-agent was its inbox row, if it had one.
2. **The parent gives no sign it has children.** Its row says "Working" while it actually waits on
   a sub-agent that waits on you. The session page has nothing about its sub-agents.
3. **Approvals are buried.** A sub-agent's approval shows in the inbox under the sub-agent's own
   title with no hint which conductor it belongs to. The session page has no approval UI at all,
   so even an opened chat that waits on an approval can't be answered there.
4. **Sub-agent work in the parent is raw tool output.** The conductor's transcript is a run of
   `create_chats` / `read_chat` / `wait_for_turn` tool rows with JSON output. Nothing says which
   sub-agents exist, how they're doing, or lets you open one.
5. **No way between parent and child.** Opened from the inbox, a sub-agent page has no breadcrumb
   to its conductor and no way to reach its siblings.

## Design

Phone-first: one column, glanceable status, big targets, waiting-on-you first.

### 1. Sessions strip (top of the session page)

A horizontally scrolling row of glass cards pinned under the navigation bar, shown on any session
that has sub-sessions or a parent:

- **Leading card: the parent.** On a sub-agent it reads "‹ Conductor title" (the breadcrumb back).
  On the parent itself it reads "Main" and is selected (a sub-session that has its own
  sub-sessions shows "‹ its parent", then itself, then its children).
- **Then the sub-sessions**, waiting-on-you first, then working, failed, done, idle; ties by most
  recent activity. Each card: a status dot (spinner while working), the title, one line of latest
  activity (the chat's last-message preview, or "Needs approval: Run the offline UI tests"), a
  badge with the number of pending approvals/questions in it or below it, a "↳ 2" count when it
  has its own sub-sessions, and the host's name when it runs on another device.
- **The selected card** is the page you're on, so the strip doubles as the segmented control:
  tapping a sibling replaces the page (no growing back stack), tapping the parent pops back to it
  (or inserts it under the page when you came from the inbox), tapping a child pushes it.
- **"All N" card** at the end when there are more than four sub-sessions: a sheet listing every
  sub-session in one column, grouped Needs you / Working / Failed / Done, big rows.

Why a strip and not a segmented control or swipe paging: a segmented control can't hold ten
names, and horizontal swipes on the transcript collide with the back-swipe and with sideways
scrolling code blocks. The strip keeps status for every sub-agent on screen while you read.

### 2. Waiting on you, first

- **Approval panel above the composer.** When the page or anything below it waits on an approval
  or question, a glass panel shows the oldest one: which sub-agent asks ("Offline banner and retry
  states asks"), the request title, the exact command in the risk colour, and **Approve /
  Decline** (one tap, same path as the inbox: `respondAttention` on the item's host). Questions
  show **Open** instead. "1 of 3" steps through the rest. This also gives a plain chat that waits
  on an approval a way to answer it from its own page.
- **Inbox breadcrumb.** A sub-agent's inbox row shows its conductor above the title
  ("Ship offline mode for iOS ›"), so you know what you're approving.
- **Rollup on the parent's row and on Mission Control.** A parent whose sub-agent waits on you
  reads "Needs you" in the sessions list and lands in Needs you on Mission Control (like the
  desktop conductor card), and its row shows how many sub-sessions it has.

### 3. Summary cards in the parent transcript

Each direct child gets a card in the parent's transcript, placed after the last message written
before the child started (the desktop's `anchor_entry` rule), so it sits right after the
`create_chats` call that spawned it. Collapsed (default): status dot, title, status word and time
("Working · 4m", "Done · 3m ago", "Failed · 7m ago"), one line of latest activity. Tap the chevron
to expand: host and model, run time, sub-sessions below it, and up to four lines of its last
message, plus **Open**. Tapping the card opens the child.

Layout stays in Rust (the transcript is laid out by `crates/mobile`): the app hands the
transcript the child slots (`TranscriptView.setChildCards`: chat id, start time, line count) and
the layout engine places a row of that height with a `childCard` widget; Swift paints the card
natively in that rect, the way agent visuals are hosted. Status changes only repaint the card;
only expanding/collapsing or a new child re-lays out the transcript.

Tokens aren't on the card: the registry row carries no token counts, and reading them means
opening every child's session doc (ten documents for a ten-agent conductor). The expanded card
shows run time and model instead. Adding a token total to the registry session row is the
follow-up if the owner wants it.

## Data

No new server or wire fields. Everything is already synced to the phone:

- the tree: `SessionRow.parentChatId` and `CoreClient.childSessions(parentId:)`;
- status: `SessionRow.indicator` (staleness-gated), `workingSinceMs`, `lastActivityMs`, `preview`;
- approvals: the merged attention queue (`AttentionItemView.chatId`), mapped to their sub-agent
  by walking the parent links.

The only FFI addition is layout-side (`ChildCardSlot`, `TranscriptView.setChildCards`,
`RowKind.childCard`, `WidgetKind.childCard`), and it's additive. `childSessions` now leaves out
archived children, like the desktop's lanes.

## Edge cases

- **10+ children.** The strip scrolls; the "All N" sheet lists everything. The transcript cards
  are per direct child and collapsed by default (one card ≈ two lines).
- **Deep nesting.** Conductors nest up to three levels. A card shows "↳ N" for sessions below it;
  opening it shows its own strip with a breadcrumb to its parent. Approvals roll up through every
  level to the root's panel and row. The walk is bounded (depth 4, visited set) so a bad parent
  link can't loop.
- **A child on another device.** Its card names the host ("on hetzner-01"); approving goes to that
  host (`AttentionItemView.deviceId`). An offline host greys the card's host label and the core's
  derived inbox rows (open-only) apply.
- **A child finishing while you look.** The strip and cards follow workspace changes (coalesced
  per run-loop turn); the card's dot turns to a check and the panel drops the answered item. Card
  height doesn't change with status, so the transcript doesn't jump.
- **Offline.** Everything shown comes from the synced replica, so the strip and cards still show
  the last known state; Approve fails with the existing "isn't reachable" toast and the row comes
  back.
- **Archived children** are left out of the strip and cards. A chat with no parent and no
  children shows no strip at all.

## Not in this change

- Opening a Task-tool sub-agent (its doc isn't served to the phone).
- Token totals per child (see above).
- Conductor rows themselves (`conductors` registry kind): the phone infers the tree from parent
  links, which covers side chats and MCP-created children as well.

## Alternatives considered

- **Push a child list screen** ("4 sub-agents ›" row opening a list): one more tap for every
  check-in, and no status visible while reading the parent.
- **Interleave child transcripts in the parent**: unreadable on a phone and expensive (every
  child's doc open).
- **Show children as rows in the Sessions list under their parent**: the desktop deliberately
  keeps them out of the sidebar; on the phone it would bury top-level chats under conductors.
