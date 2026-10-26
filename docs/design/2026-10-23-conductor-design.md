# Conductor: one long-lived agent that runs the work

Status: draft for review, 2026-10-23.

"Conductor" is the working name (see "Naming"). In this document:
- a **conductor** is the feature's unit: one long-lived main chat bound to a folder;
- its **main agent** is the agent the user talks to;
- **sub-agents** are the chats the main agent (or another sub-agent) starts.

## Intent

Ongoing work usually spans several kinds of tasks (reviews, features, debugging, releases) and
often several repos. Today each task means opening the right repo, starting or finding the right
chat, and switching between chats.

A conductor gives that work **one place to talk**: a single long-lived agent. It can cover anything
long-running, such as a set of repos, a single project, a feature or a migration. The user tells
it what they need and it sends the work to the right place, either an existing sub-agent that
already owns that work or a new one in the right repo with a goal. The user watches sub-agents work
from that one chat, gets a short summary when each finishes, and can step into any sub-agent's chat
whenever they want.

The idea came from Cursor's Projects (screenshot in the brainstorm: a project that "tracks the
decisions, agent work, and follow-through", with notes and memory across sessions).

## Goals

- Create **any number of conductors**, each from **one folder**: a single repo, or a parent folder
  that holds several repos.
- Conductors may overlap. A conductor for a whole folder of repos and one for a single feature can both cover the same
  repo. Each sub-agent chat belongs to exactly one conductor.
- One **main chat** per conductor that lasts for weeks. The user never has to open or switch threads
  to get work done.
- The main agent **starts, reuses and steers sub-agents** in the conductor's repos. Sub-agents may
  start their own sub-agents.
- The user **sees sub-agents working** from the main chat, gets a **short summary** when each
  finishes, and can continue either in the main chat or in the sub-agent's own thread.
- Sub-agent **questions and approvals reach the user from the main chat** without hunting.
- When any chat in the conductor nears a full context, it **rolls over** to a fresh chat without the
  user managing it.
- The conductor **learns how the user works** from their existing chats in those repos, into notes
  the user can see and edit.

## Non-goals (this design)

- A per-conductor Mission Control board.
- Reaching a conductor from Telegram.
- Running work on more than one device. Delegation, wake-ups and the attention service are all
  same-device today, and v1 keeps that.
- **Sandboxing.** Decided with the user: a conductor organizes and routes work; it does not fence
  agents in. Agents keep the full set of Godspeed tools they have today.
- Replacing ordinary chats. Spaces, the Sessions sidebar and normal chats keep working as they do.

## Anti-goals (builder's reading, to be confirmed)

The user didn't state these; they follow from what they asked for.

- The main chat turning into a wall of sub-agent output. It should read like a lead reporting to
  the user, not a log.
- The user having to manage threads, context limits or which chat is which. If they find
  themselves doing that, the feature has failed even if it technically works.
- A sub-agent in the tree being started with more access than the user granted the main agent.

## Shipping order

Three pieces, shipped in this order. Each is usable by itself.

1. **The conductor and its one chat**, including nesting, live cards and the question pop-up.
2. **Rollover** of sub-agents and the main agent.
3. **Learning from past chats** into the conductor's notes.

---

## Piece 1: the conductor and its one chat

### Creating a conductor

1. The user picks a folder: a parent of several repos, e.g. `~/code/acme`, or one repo, e.g.
   `~/code/acme/api`.
2. Godspeed finds the git repos inside it. If the folder is itself a repo, the conductor has that
   one repo.
3. Optionally, the user gives it a name and a one-line purpose, e.g. "Ship the billing rewrite by
   December" or "Keep the API and web app healthy". The purpose goes into the main agent's
   instructions. Builder's call, flagged for the user.
4. The user picks the main agent: harness, model and access mode. Defaults come from the user's
   usual new-chat defaults.
5. The user lands in the main chat. Notes start empty in piece 1 (piece 3 drafts them).

Decided with the user:
- Setup is just the folder and the agent. No listing repos, no writing instructions up front.
- The agent choice can be changed later (see "Changing the main agent").

Builder's calls:
- Each discovered repo becomes (or reuses) an ordinary space on this device, so sub-agent chats are
  normal chats in normal spaces, with diffs, branches and worktrees working as they do today.
- Repos added to the folder later are picked up when the conductor is opened or rescanned. A file
  watch is optional.
- Only repos inside the folder belong to the conductor. Its main agent is told to work within them.
  This is guidance, not a fence (see Non-goals).
- Discovery depth (direct children only, or deeper). Direct children plus the folder itself covers
  the common layout.

### Where it lives in the app

- Each conductor gets its own entry in the sidebar, separate from the Sessions list. Clicking it
  opens the main chat as a tab. (Builder's call on the exact placement; a section above Sessions
  matches the screenshot.)
- The entry shows "needs you" when anything anywhere in its tree is waiting on the user.
- Conductor settings hold the folder, the main agent (harness, model, access mode), the repo list
  and the notes.
- Sub-agent chats stay hidden from the Sessions sidebar, as child chats are today. They're reached
  through their cards, "Open thread", the existing lanes card, and deep links.

### The main agent

- The main chat is a normal top-level chat that belongs to the conductor.
- It's driven by the chosen harness and has Godspeed's MCP server like every run, plus
  conductor-aware abilities:
  - list the conductor's repos;
  - start a sub-agent in a repo with a short title and a goal (e.g. "PR 569 review",
    "Release 2.4");
  - find the existing sub-agents and what each owns, and send work to one instead of starting a
    new one;
  - read and edit the notes.
- Its instructions say what a good report looks like: a short summary with the verdict and the one
  or two things that matter, and what the user can do next. It says where it sent the work when it
  starts it.
- Godspeed has no system-prompt field on runs (`RunRequest`), so the conductor's instructions and
  notes go in front of the first prompt of each fresh harness session. Always-on agents do the same
  with `MEMORY.md` (`crates/engine/src/agents/prompt.rs`), and account handoff does the same with
  carried context.

Builder's calls:
- Whether reuse works only from the agent reading the titles and goals of existing children, or the
  engine also keeps an explicit "what this sub-agent owns" field. Titles plus goals is the simpler
  start.
- Sub-agents default to the conductor's harness and model. The main agent may pick another per task
  if the notes say so (e.g. "reviews on Claude").

### What the main agent does itself

Builder's call, flagged for the user: the main agent **reads but delegates changes**.
- It may answer quick questions itself, such as "where are we on the auth migration?", using the
  notes, its sub-agents' chats and read-only looks at the repos.
- Anything that edits code, runs builds or touches a remote (PR comments, releases) goes to a
  sub-agent.
- Its working folder is the conductor's folder.

### Sub-agents, including nesting

Decided with the user: there are **both one-off and continuing sub-agents** ("continuous or not").
The main agent decides which a request needs:
- A one-off, like "PR 569 review", is archived some time after it finishes and its summary has been
  given.
- A continuing one, like "Releases" or a long feature, stays available for reuse.

Builder's call: how long finished one-offs are kept before archiving. The goal is to keep the list
of reusable children small over a weeks-long conductor.

Decided with the user: **sub-agents may start their own sub-agents.**

- Today only top-level chats can be parents (`crates/mcp/src/tools.rs` around 895-945,
  `crates/engine/src/delegation.rs:320` and `:419`, `docs/mcp.md` "Parent links"). Inside a
  conductor that limit is lifted.
- **Access never widens going down.** Each child's access is at most its parent's, as delegation
  already enforces.
- **Depth cap** so a confused agent can't fan out without end. Builder's call; 3 levels below the
  main agent is the suggested value.
- **Wake-ups go to the direct parent**, not the main agent. A nested sub-agent finishing or failing
  wakes the sub-agent that started it, which decides what to report upward. The main chat stays at
  the level the user asked about.
- Ordinary chats outside a conductor keep today's one-level rule, unless the builder finds it
  simpler to lift it everywhere.

### Live cards in the main chat

Decided with the user (mockup "A" in the brainstorm, without a text box):

- When the main agent starts a sub-agent, a **live mini-card** appears in the main chat. It shows:
  - name and harness;
  - status and elapsed time;
  - the last ~3 steps as they happen (e.g. "✓ Read src/billing/invoice.rs", "✓ Ran cargo test",
    "● Checking migration 0042…");
  - an "Open thread" link.
- The card has **no text box**. To type or steer, the user opens the thread, or tells the main
  agent, which passes it on.
- When the sub-agent finishes, the card settles to a one-line done state and the main agent's
  summary follows it.
- A card shows a small count of nested work under it (e.g. "+2 sub-agents"). Opening the thread
  shows that sub-agent's own cards for its children, so the tree is browsed one level at a time.
- The main chat shows cards only for sub-agents the main agent started itself.
- Sending more work to an existing sub-agent either shows its card again at that point in the chat
  or re-activates the earlier card. Builder's call on which.

Existing pieces to build from:
- Native harness subagents already get a live spawn chip with a one-line tail
  (`crates/ui/src/transcript.rs`, `subagent_tail` in `crates/doc/src/parts.rs`). MCP-started chats
  currently render as a generic tool chip.
- Turn receipts (`crates/ui/src/receipt.rs`, `Row::Receipt`) are a custom transcript row with its
  own cache that opens in place: the right shape for a card row.
- The lanes card (`crates/ui/src/shell/session_lanes.rs`) already watches a chat's direct children
  with live status.

### Questions and approvals: the "Answer" pop-up

Decided with the user:

- When a sub-agent needs an answer, a choice or a permission, its card turns **amber** and shows a
  button such as "Reviewer has a question · Answer".
- The button opens a **pop-up** with the question and its options (or Allow/Deny for
  permissions). The user picks and sends.
- Wanting to say more means opening the thread. The pop-up says so.
- **Questions from nested sub-agents surface on the top-level card in the main chat that leads down
  to them.** The pop-up names the path, e.g. "PR 569 review → migration tester".

**Who may answer.** Decided with the user: parent agents may answer **low-risk approvals only**.
"Low risk" is the existing `ApprovalRisk::Low` band (`crates/proto/src/approval_risk.rs`).
Everything else waits for the user: medium- and high-risk approvals, and every question.

Hardened in review: the rule keys on the chat being answered, so any agent (a conductor member or
not) answering a conductor sub-agent gets it; agents may only approve once (no approve-for-session
or approve-always); a failed membership lookup refuses the approval. Conductor members can't start
standalone chats or parent chats outside their tree, and a conductor runs at most 8 sub-agents at
once. Parent wake-ups inside a conductor are limited to 20 a minute.

Builder's calls on the details:
- The parent is still told when a child is waiting on the user, so it doesn't stall confusingly,
  but it can't answer those items.
- When an agent answers a low-risk approval, the card and thread show what was approved and by
  which agent.
- The first answer wins. A late answer from the user or an agent gets "already answered".

Existing pieces and gaps:
- The approval card body is already a shared, pure function (`crates/ui/src/approval_view.rs:19`,
  used by the composer and the Needs-you inbox). It can go in the pop-up as is.
- Question forms (pick an option, multi-field) are composer-only (`render_wizard` in
  `crates/ui/src/composer.rs`). They need the same extraction.
- There's no modal primitive yet; there's an anchored popover (`crates/ui/src/popover.rs`).
  Builder's call whether the pop-up is a modal or an anchored popover.
- The attention service (`crates/engine/src/attention.rs`) already sees child chats' pending
  approvals and questions, but items don't link to their parent or top-level ancestor. That link is
  needed so the right card lights up.
- `respond_to_input` today lets a parent answer anything up to its own access mode
  (`crates/mcp/src/tools.rs`). Inside a conductor it must be limited to low-risk approvals.
- Child chats deliberately make no sound or banner today (`crates/ui/src/shell.rs` around 3140).
  Builder's call: inside a conductor, one banner per waiting item, named by conductor, seems right,
  since the user isn't watching the child chat.

### Notes (piece 1 version)

- Each conductor has a notes file that the user and the main agent can both read and edit. The user
  edits it from conductor settings.
- The notes go in front of the main agent's first prompt in each fresh session. Sub-agents get them
  too, since review standards and release steps are exactly what they need. Capped at 12 KB (see
  below).
- In piece 1 the notes are written by hand, by the user or the agent. Piece 3 adds learning.

Decided while building piece 1: the notes live **on the synced conductor row** (a `notes` string,
capped at 12 KB), not in a file. The registry room rejects any change over 16 KiB, and a rejected
batch wedges sync, so every conductor write is refused up front when the row it leaves behind
(serialized, with its clocks) would not fit; names (120 bytes) and purposes (500 bytes) are capped
for the same reason. They then sync with the rest of the registry and need no data-dir
plumbing; the cost is that they are not a file on disk. The engine reads them through
`GetConductorNotes` / `SetConductorNotes`, and the main agent through the `conductor_notes_read` /
`conductor_notes_write` MCP tools.

### Changing the main agent

Decided with the user: keep it simple and reversible. The agent is chosen at creation and can be
changed later.

- The change takes effect as a rollover (piece 2): the main chat continues in a fresh session on
  the new agent, with the notes and a summary, behind a divider like "Switched to Codex".
- Running sub-agents keep their harness and model; only new work uses the new choice.
- Access works differently. If the user lowers the main agent's access, every sub-agent is clamped
  to the new level from its next turn. A turn already running finishes first. This keeps "access
  never widens going down" true without exceptions. (Builder's call, flagged for the user.)
- Until piece 2 ships, changing the agent applies from the next fresh session of the main chat.
  Builder's call on how to present that.

---

## Piece 2: rollover

- When a sub-agent or the main agent nears a full context, it continues in a fresh chat carrying a
  summary.
- **To the user, a rolled-over chat still looks like one chat**: a quiet divider, and the same card
  in the main chat keeps tracking the work.
- The conductor's sidebar entry always opens the latest main chat.
- The main agent's own rollover works the same way, and so does changing the main agent.

Existing pieces:
- Context usage is tracked per chat (`meta.contextUsage`, `crates/doc/src/schema.rs`), but only
  the UI reads it. `turn_stats` (`crates/doc/src/turn_stats.rs`) is written by the engine on each
  turn.
- Account handoff (`crates/engine/src/account_handoff.rs`) already:
  - creates a new chat with a fresh harness session;
  - builds a summary (`continuation_brief`, a 6,000-character extract with no model call);
  - stores it as carried context, put in front of the first prompt;
  - shows a "Forked from" divider with the copied transcript.

Rollover also needs it to:
- stay on the same account;
- keep the parent link;
- move the card;
- ideally use a model-written summary.

Builder's calls:
- the threshold (e.g. ~75–80%, leaving room for a model-written summary);
- rollover only between turns, never mid-turn;
- what to do for harnesses with no context reporting (Cursor);
- whether agents can also ask for a rollover themselves.

---

## Piece 3: learning from past chats

- When a conductor is created, and from time to time after that, it reads the user's existing
  Godspeed chats whose working folder is inside the conductor's folder. Those chats may come from
  any harness, since Godspeed stores them all.
- From them it works out how the user operates: how they review PRs, release steps, which repo owns
  what, conventions they keep correcting agents on.
- It **proposes additions to the notes for the user to accept**. Nothing goes in silently.
- New chats, including the conductor's own sub-agents', feed later passes.
- Builder's call, flagged for the user: it reads **only chats already in Godspeed**, which is what
  the user described ("the chats in the workspace"). Importing native CLI history that was never
  run through Godspeed is out of scope; it would need a hidden, analysis-only import path so the
  Sessions sidebar isn't flooded.

Existing pieces:
- the full-text message index (`crates/engine/src/message_index.rs`, `SearchMessages`), which isn't
  yet exposed to agents;
- the chat importers (`crates/engine/src/chat_import/`), which bring in Claude, Codex, Cursor and
  T3 history.

Builder's calls:
- how often passes run: on creation, on demand, and periodically via the always-on agent machinery;
- what the proposal-review UI looks like.

---

## Naming

Decided with the user: the feature is called **Conductor** for now, and the name must be **easy to
change later**.

- The user-facing name (sidebar section, settings, menu items, copy) comes from one place, so a
  rename is a one-line change plus copy review.
- Persisted and wire identifiers are chosen so a display rename never forces a migration or breaks
  the MCP surface: registry row names, file paths, RPC and MCP tool names. Builder's call: either
  neutral internal names, or `conductor` internally with the understanding that it stays even if
  the display name changes.
- The existing use of "project" for a space is left alone: `list_projects` and the `project`
  argument in `crates/mcp/src/tools.rs`, `project_key()` in `crates/proto/src/view.rs`, and "add a
  project to another device".

## Constraints

- Same device only in v1 for the work: the main agent and every sub-agent run on the device that
  holds the folder. Builder's call, flagged for the user: the conductor itself lives in the synced
  registry like spaces do. That way the main chat can be watched and answered from another device's
  Godspeed, or the iPhone app, the same way any chat can.
- Access caps flow down the tree for every chat a conductor starts. Since the user chose not to
  sandbox, this is not a guarantee against an agent using other chats on the machine.
- Normal chats, spaces and the Sessions sidebar behave exactly as before for anything outside a
  conductor.
- Godspeed never resells model access. Every agent in the tree runs on the user's own signed-in
  CLIs.

## Left to the builder

Everything marked "builder's call" above. In addition:

- the data model for a conductor (a registry row like spaces) and how it references its repo
  spaces;
- the exact MCP tool shapes for the conductor abilities;
- how the main chat's long-running cards are cached and kept live across app restarts;
- tests, migration of existing chats (none expected), and docs updates: `docs/mcp.md`,
  `ARCHITECTURE.md`, and `CONTEXT.md` vocabulary for "conductor", "main agent" and "sub-agent".
