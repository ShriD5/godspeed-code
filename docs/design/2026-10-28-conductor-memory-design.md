# Conductor and Zoomie memory: remember accurately, stay fresh

Status: draft for review, 2026-10-28.

Builds on the conductor design (`docs/design/2026-10-23-conductor-design.md`) and on standing
instructions (PR #44, `feat/conductor-instructions`). It **revises piece 3** of the conductor design
(learning into notes) and **builds the main agent's half of piece 2** (rollover, §9a). Sub-agent
rollover is unchanged.

**One memory engine, two users (decided with the user).** The memory described here is a shared
engine piece. Conductors and Zoomies (always-on agents) both use it. Zoomies replace their free-text
`MEMORY.md` with it. Most of this document is written from the conductor's side. §11 covers what
changes for Zoomies.

In this document:
- **standing orders** are the conductor's instructions from PR #44, owned by the user;
- **memory** is what the conductor has learned: facts about the project, the work and the user;
- a **memory entry** is one fact in memory, with where it came from and when it was last confirmed;
- **wake-up** is what the conductor does when it picks up again after any gap.

## Intent

The user wants the conductor to be better than an ordinary chat because it runs for weeks without
losing the plot. The products they named as inspiration (xAI Grok Bot, OpenAI Dots, Meta Muse) feel
good when they "remember things correctly and accurately even as the chat continued."

Their worst experience with long chats wasn't forgetting. A long chat "zeroed in on a bad approach
without considering new things or updated things," while the same bug described in a new chat was
solved quickly. A long context isn't only lost information. It also piles up the agent's own old
theories, and the agent anchors on them.

So the conductor needs two things that pull against each other:
- **Remember accurately**: what the user said, decisions, what was tried and how it went, how the
  user works.
- **Stay fresh**: never treat a memory as true just because it is old or often repeated, and
  re-check what it knows against the world before acting on it.

The user ranked **memory you can check** first. The other half of freshness (fresh worker chats and
spotting stuck workers) is a separate, later design (see Non-goals).

"Check" has two halves, and this design covers both:
- **The conductor checks itself.** Before relying on a fact, it confirms the fact against the repo,
  git history or what the user said. When the world has moved on, it corrects itself.
- **The user can check it.** They can see what it believes, where each belief came from, and which
  beliefs are guesses, and they can fix any of them.

### What the research says (2026-10-28)

The brainstorm drew on three research passes. Grok Bot, Dots and Muse launched between August and
September 2026; findings about them come from press, reviews, HN and GitHub issues, not vendor docs.

- **Hidden memory is the top complaint about Grok Bot, Dots and Muse.** With Grok Bot and Dots you
  can't view, fix or delete individual memories, and deleting everything is the only reset.
  A Dots GitHub issue (openai/codex#57536): "the next scheduled run still behaves as if it has never
  seen that instruction before." A Dots reviewer said its memory "began describing an older version
  of how they work." Muse's "forget" is best effort.
- **The anchoring the user saw is a measured effect.** Laban et al., "LLMs Get Lost In Multi-Turn
  Conversation" (arXiv 2505.06120): models anchor on early assumptions and "when they take a wrong
  turn… do not recover." Their advice is a new conversation with the facts restated. Claude Code's
  best practices say the same: after two failed corrections, clear and re-prompt.
- **Summary on summary drifts.** Amp removed compaction in favour of handing off to a fresh thread
  (ampcode.com/news/handoff). Claude Code compaction issues report lost specifics and redone work.
- **Check-on-read is the best memory design found.** GitHub Copilot memory stores each fact with
  citations to code. Before use it re-checks the citations against the current branch, corrects a
  fact the code contradicts, and deletes a fact unused for 28 days. "Hard to solve, but easy to
  verify." PR merge rate rose from 83% to 90% (github.blog, agentic memory for Copilot).
- **Small, readable memory earns trust.** Hermes Agent keeps small capped memory files plus search
  over raw past chats, and gets the best memory reviews. Its worst complaint is the agent overwriting
  the user's manual edits.
- **Memory learning from its own output pollutes the store.** A mem0 user's audit found a made-up
  "user prefers Vim" recalled and re-saved about 800 times (mem0 issue #4573; the numbers are
  unverified, but the loop it shows is real).
- **Memory can make agents agree with the user even when the user is wrong.** Stored beliefs should
  not beat evidence.

## Goals

- The conductor **remembers accurately for weeks**. Each memory entry says where it came from,
  whether it was seen or guessed, and when it was last confirmed.
- **It knows how long it has been away** and, on wake-up, re-checks what it knows against the repo,
  docs, git history and other chats before doing anything else.
- **It assumes it is not the only one working.** The user, other agents and CI change the repo. It
  adapts to their changes and never undoes them.
- **Older memories count for less.** Weight comes from when a memory was last confirmed, not when it
  was first saved. Unconfirmed memories fade, get re-checked, and eventually drop.
- **Saving is visible.** Facts are saved with a quiet line in the chat and one-tap Undo. Guesses,
  especially about the user, ask first.
- **The user can read, edit, delete and undo** anything in memory, and the agent never overwrites
  their edits.
- **Zoomies get the same memory.** Each run checks what changed since the last run, so a scheduled
  run never acts as if it has never seen an instruction before. That is the top complaint about
  OpenAI Dots.
- **Creating a conductor is its first wake-up.** "Everything" becomes a grounded remit built from
  the repo, docs, git history and the user's chats in that folder. This fixes the problem raised on
  PR #44.

## Non-goals (this design)

- **Shared or team memory.** Decided with the user: a conductor is theirs, for now.
- **Fresh worker chats and stuck detection.** This is the next design. It builds on this one, since
  good worker briefs come from good memory.
- **Replacing harness-native memory** (Claude Code's CLAUDE.md and auto memory, Codex's AGENTS.md).
  The conductor reads repo files like these as evidence; it doesn't manage them.
- **Profiling people other than the user.** Teammates appear only as authors of changes ("commit
  abc by Shri"), never as personal profiles.
- **Running memory work on another device.** Same device as the conductor, as in v1.

## Anti-goals

What would make this a failure even if it technically works. The first four come from the user or
the research; the rest are my reading.

- **Memory the user can't see into.** This is the Grok Bot and Dots trap.
- **Old memories beating fresh evidence**, so the conductor anchors on stale facts or old dead ends.
- **Asking so often that the user taps yes without reading.** Dots users report exactly this.
- **The agent overwriting the user's edits** to memory or standing orders.
- Saying "saved" for a write that didn't happen.
- Memory learning from its own output, so one guess becomes a "fact" by repetition.
- A confident biography of the user inferred from commit history.
- Someone else's PR comment or doc text ending up as a standing instruction, or widening what the
  agent may do.
- Re-checking that silently burns the user's subscription limits.

## How it behaves

### 1. What may enter memory

Only three sources produce memory entries:

- **What the user said** in the conductor's chats.
- **What the repo shows**: code, docs, git history, PRs and CI results. The entry cites the file
  and lines, commit or PR.
- **Outcomes that were checked**: a test that passed or failed, a PR that merged, a fix that held.

Everything else is a **guess** and is labelled as one. This includes a worker's "I think the cause
is X" and a pattern the conductor infers. A guess becomes a fact only when one of the three sources
confirms it.

**Sub-agents don't write memory.** They report to the main agent, and only the main agent writes,
through Godspeed's tools. A worker's reported outcome counts as **checked** only when evidence the
engine can see backs it. Examples: a command and its exit code in the worker's transcript, a PR's
merged state, a CI result. Without that evidence, the outcome is saved as a guess. (My call.)

The conductor never extracts memory from its own earlier replies or from memory text it was given.
Re-reading memory doesn't make it truer.

**Content from other people is information, never instructions.** PR comments, issues, docs and
other chats can create entries ("the README says releases are on Fridays"). They can't create
standing orders or widen what the agent may do. The same goes for the user's one-off permissions:
"go ahead and push" for one task doesn't become "always push."

### 2. Saving visibly (decided with the user: option C from the mockups)

- **Facts are saved right away and shown.** These are what the user said, what the repo shows, and
  checked outcomes. A quiet line in the transcript reads "Remembered *fact* · *source*", with Undo.
  It never interrupts.
- **Guesses about the user ask first.** Technical guesses, like a worker's suspected cause, are saved
  and shown labelled "guess" without a question. They get confirmed by later evidence or fade fast.
  Asking about every technical guess would flood the user. (My call, refining option C.) Guess
  questions appear as an inline question
  ("It looks like you review PRs yourself before merging. Is that right?") with Yes / No / Not
  quite…. "Not quite…" lets the user correct it in their own words. An unanswered guess is kept as a
  guess, never promoted.
- **A save that failed says so.** No "Remembered" line for a write that didn't happen.
- **On other devices** (another Mac, the iPhone app), the "Remembered" lines show read-only, and guess
  questions can be answered by replying in the chat. Undo and the buttons work on the device that
  hosts the conductor. Full remote Undo is later. (My call.)

The sketches are in `.superpowers/brainstorm/` (not committed): `memory-saves.html`, option C.

### 3. Using memory: check before relying

- Before a memory entry drives a decision, the conductor checks it against its source if it hasn't
  been confirmed recently (§4). Code-backed entries are checked against the current branch.
- **Fresh evidence always wins.** When the world contradicts memory, the conductor writes the
  corrected entry, keeps the old one as superseded (with when and why), and says so briefly.
- A remembered user preference never overrides evidence on a question of fact.
- In replies, the conductor separates what it knows from what it's guessing. When a source has
  changed and it hasn't re-checked yet, it says "I'm not sure any more" rather than reciting the old
  fact.

### 4. Older memories count for less

- **Every entry has a "last confirmed" time.** It moves forward only when something re-confirms the
  entry: the repo still matches, the user says it again, or a check still passes. Simply using an
  entry doesn't refresh it. This stops an entry staying alive just because the conductor keeps
  citing it.
- **The longer since confirmation, the less an entry counts.** An entry past its kind's freshness
  window is shown as possibly stale and must be re-checked before it can drive a decision. One past
  its expiry is removed (kept in history, §8).
- **Entries the user stated, confirmed or edited never expire on a timer.** They dim and get
  re-checked like any other entry. When one looks wrong, the conductor asks before dropping or
  changing it. Only agent-sourced entries are removed automatically. (My call; resolves the tension
  with "never overwrite a user edit.")
- **Kinds fade at different speeds.** These are my guesses; the numbers are the builder's call:

  | Kind | Example | Fades over |
  |------|---------|-----------|
  | Current work | "PR #44's ui-tests fails only in CI" | days |
  | Project fact | "Sync goes through the room; the v1 API is retired" | weeks, unless re-confirmed |
  | About the user (confirmed) | "Prefers blunt reviews" | slowly |
  | Ruled out | "Tried X on bug Y; failed with error Z" | until the code it depended on changes |
  | Standing orders | — | never; the user owns them |

- **Ruled-out approaches reopen when their basis changes.** A ruled-out entry records the files or
  commit it depended on. When those change, the entry is marked "may be worth retrying" instead of
  blocking the approach forever. This directly targets the user's pain of a chat stuck on an old
  conclusion.

### 5. Waking up after a gap

- **The conductor keeps a "last seen" bookmark**: the time, the HEAD commit of each repo, the PRs and
  CI runs it knew about, and the newest Godspeed chat in its folder that it has read.
- **On every message from the user, the engine compares now with the bookmark.** This is a cheap
  check with no model calls: git HEADs, file timestamps on the docs it relies on, CI status and new
  chats in the folder. Turns started by sub-agent results or notices don't trigger it.
- **When the cheap check finds material change, the model work runs as a separate job, not in the
  main chat's context.** A hidden side run re-checks the changed sources, updates memory and hands
  the main agent a short digest. This keeps the re-check reasoning out of the main chat, where it
  would build up the anchoring this design exists to avoid, and lets its usage show separately. The
  main agent can acknowledge the user straight away ("Catching up on what changed since Tuesday…")
  and answers once the digest lands. The depth scales with the gap and the amount changed. The
  thresholds are the builder's call. (My call.)
- **Nothing runs while the user is away.** Wake-up starts when they come back, so it never spends
  their limits unseen. A scheduled pre-warm could come later as an opt-in.
- **What it re-checks:**
  - new commits and merged PRs, and who made them;
  - changes to the docs it relies on: README, specs, progress trackers, AGENTS.md, CLAUDE.md;
  - CI state on branches it cares about;
  - every memory entry whose source changed (entries whose source didn't change stay trusted, so a
    re-check doesn't redo everything);
  - **other Godspeed chats that ran in the same folder**, including ones outside this conductor.
    Godspeed can see these, which Grok Bot and Dots can't, so the conductor knows what other agents
    did, not just what they committed.
- **It isn't the only one working.** It notes who changed what. If someone else fixed what it was
  working on, or took a different direction, it adapts and never undoes their work. Unfinished tasks
  from before the gap are treated as possibly stale until checked.
- **"While you were away."** After a real gap it posts one short note covering three things: what
  changed and who changed it, what it corrected or dropped in memory, and what it's now unsure
  about. Then it carries on. If nothing material changed, there's no note.

### 6. Creating a conductor is the first wake-up

The PR #44 feedback: the user typed "Everything" for the conductor's responsibility, and that word
was pasted into a fixed template. Creation should instead run the wake-up with an empty bookmark,
then draft the standing orders from what it found:

- **Responsibility.** "Everything" means broad ownership of this folder's repos. That covers
  coordinating development, maintenance, investigation and reviews.
- **Project understanding.** What the product does, its main parts, how work is organised. This
  comes from READMEs, docs, the code layout and recent history.
- **How the user works.** This comes from the user's Godspeed chats in this folder, including chats
  imported from Claude Code, Codex, Cursor and T3. Inferences go into memory as **guesses** and are
  asked about (§2). They don't go into the standing orders as facts.
- **Boundaries.** A broad remit isn't permission to act. Deploys, publishing and anything hard to
  reverse still wait for the user.

**How it feels.** The sheet doesn't wait. The user lands in the conductor right away, and drafting is
the main agent's first turn, shown as "Understanding this repository…". For that turn only, the agent
may write the standing orders. If the user edits the orders while the draft is still being written,
the user's edit wins and the draft is offered as a suggestion. Existing conductors with template
orders aren't redrafted automatically. They get a "Redraft from the repo" action. (My call.)

Volatile context, such as the current branch, open PRs and recent activity, stays in memory and live
context so it can fade. It isn't frozen into the standing orders. The user lands in the conductor
with a short explanation: "I've drafted my responsibilities from your request and what I found in
the repo," and can edit both the orders and the memory.

This replaces piece 3's "propose additions to the notes for the user to accept." Under option C,
facts are saved and shown, and guesses ask. Later learning passes (for example, after new chats in
the folder) follow the same rules.

### 7. Notes, standing orders and memory

Today the conductor has two text fields on its synced row: `instructions` (standing orders) and
`notes` (written by both the user and the agent through `conductor_notes_write`). With memory:

- **Standing orders** stay user-owned. The agent drafts them only at creation. After that, only the
  user edits them (the PR #44 flow).
- **Memory** is where everything the agent learns goes.
- **Notes become the user's own scratchpad**, and the agent stops rewriting them. The agent can still
  read them. Anything the agent used to write into notes becomes memory entries. Existing agent-written
  notes stay as they are. (My call; the alternative is to retire notes into memory entirely.)

### 8. Seeing, editing and undoing memory

- A **memory view** for each conductor lists entries by kind. Each entry shows its source, its
  status (seen or guess) and when it was last confirmed. Possibly stale entries are dimmed. The user
  can edit, delete, confirm a guess, or mark an entry wrong. (Where the view lives is the builder's
  call. My suggestion: next to "Edit instructions" in the session card.)
- **The agent never overwrites a user edit.** If evidence later contradicts an entry the user
  edited, the conductor raises it and asks rather than changing it.
- **History.** Every change to memory is kept, so the user can ask "what did it believe last
  Tuesday?" and undo a bad write. The "while you were away" note can link to the memory diff.
- Edits made by the user mid-session reach the running agent the same way instruction edits do
  (`instructions_changed_notice` in PR #44).

### 9. What the agent loads

- Every fresh session of the main agent gets the standing orders, then live context (folder, repos,
  user notes), then a **small, always-loaded memory index**. The index holds the entries that matter
  most now: fresh, confirmed, and relevant to the active work. Details are fetched on demand through
  tools. This follows Hermes's small loaded memory plus search, and keeps per-session token cost
  bounded.
- Sub-agents get the entries relevant to their brief, not the whole memory. They get facts, not the
  conductor's theories. (The worker-briefing design builds on this.)
### 9a. Fresh-session restart for the main agent (decided with the user: in scope)

Today the main chat resumes the same provider session forever. Standing orders, live context and
notes go in only on the first prompt of a fresh session (`crates/engine/src/sessions.rs`, around the
conductor prompt prefix), and piece 2 rollover has no code yet. Without this section, memory would
make the conductor more accurate but not fresher.

- **When.** The main agent starts a fresh provider session in two cases: after a **real gap** (the
  same threshold that triggers a full wake-up, §5), and when its context is **nearly full**. The
  threshold is the builder's call; piece 2 suggested about 75–80%. Restarts happen only between
  turns, never mid-turn.
- **What the fresh session gets.** It is rebuilt from the standing orders, live context, the memory
  index, the wake-up digest (if any) and the last few turns verbatim. It is not built from a summary
  of a summary.
- **Before it restarts,** the agent writes anything worth keeping to memory. Memory carries what
  matters, so nothing critical depends on the carried-over turns.
- **To the user it's still one chat.** A quiet divider ("Fresh session · caught up from memory"), the
  same main chat, the same sub-agent cards. Same account and same harness. This reuses the account
  handoff's fresh-session-with-carried-context path (`crates/engine/src/account_handoff.rs`)
  without switching account.
- **Harnesses that don't report context usage** (Cursor) restart only on the gap trigger.
- **Out of scope:** sub-agent rollover (piece 2 for sub-agents stays as designed), and agents asking
  for a restart themselves.

### 10. Same on every harness, and visible cost

- Conductor memory belongs to Godspeed, not the harness, so it behaves the same on Claude Code, Codex
  and Muse Code. Harness-native memory files in the repo (CLAUDE.md, AGENTS.md) are repo evidence
  like any other doc. When they disagree with conductor memory, the conductor re-checks and says so.
- Re-checks spend the user's own subscription. A light check (git, file timestamps, CI status)
  needs no model calls. Model work runs only on what actually changed. Godspeed already tracks
  usage, and wake-up and re-check usage should show up there. (The budget shape is the builder's
  call.)

### 11. Zoomies use the same memory engine

Today each Zoomie has `{data_dir}/agents/{id}/MEMORY.md`. It is injected into every run's prompt
(capped at 32 KB, `crates/engine/src/agents/prompt.rs`), and the agent edits the file in place. That
means its writes aren't sourced, checked, faded or shown, and the user can't tell what changed. With
the shared engine:

**Same as conductors**
- The same store, entry schema, rules for what may enter memory (§1), check-before-use (§3),
  fading by last-confirmed time (§4), history and undo (§8), and loaded index (§9).
- **Writes go through Godspeed's memory tools, not in-place file edits.** Every write is
  attributable and shows as a "Remembered" line in the run's chat.
- **The Zoomie's instructions play the role of standing orders.** They are already user-owned, and
  the agent never edits them.
- **A memory view for each Zoomie**, in Settings → Zoomies, next to the Zoomie's instructions.

**Different for Zoomies**
- **Every run is a wake-up.** Each Zoomie keeps its own "last seen" bookmark, updated at the end of
  each run. The next run starts by comparing against it: what changed in its folder, what changed in
  memory, and any edits to its instructions since the last run. Runs are already fresh chats and
  unattended, so the re-check happens inside the run. There's no main chat to protect, so no separate
  job is needed.
- **Running while the user is away is the point.** §5's "nothing runs while the user is away"
  doesn't apply. The re-check is part of the run the user scheduled, and its usage counts toward
  that run.
- **No fresh-session restart** (§9a). Every run already starts fresh.
- **Guesses about the user can't be asked mid-run.** They're saved as guesses and queued as a
  question in the Zoomie's inbox, and in Telegram if it's connected. They stay guesses until the user
  answers, and the run never waits on them.
- **No "while you were away" note.** What changed shapes the run. If it matters, it goes in the run's
  result in the inbox.

**Moving existing Zoomies over.** Existing `MEMORY.md` content is imported as agent-sourced guesses,
unconfirmed, and the original file is kept in history. The user can confirm entries in the memory
view, or let them be confirmed by evidence or fade. (My call; the exact import is the builder's
call.)

**No sharing between memories.** A Zoomie's memory and a conductor's memory stay separate, even over
the same folder, and so do two Zoomies' memories. Scoping memory hard avoids the bleed users dislike
elsewhere. Sharing project facts between them could come later. (My call.)

## Shipping order

1. **The shared memory engine.** This covers the store, entry schema, tools, check-before-use,
   fading and history, plus a memory view component both features can use.
2. **Conductors on the engine.** Saving visibly, wake-up, creation as first wake-up, notes changes
   and the fresh-session restart (§1–§10).
3. **Zoomies on the engine** (§11), including migrating existing `MEMORY.md` files.

Each step is usable by itself. (My call. Zoomies are smaller, so the builder may swap steps 2 and 3
to prove the engine on Zoomies first.)

## Constraints

- **Memory can't live on the synced conductor row.** The registry room refuses any change over
  16 KiB, and notes plus standing orders already use most of that. Memory needs its own store.
- **It must be versioned** (§8), **readable** (the user trusts what they can open), and **written
  only through Godspeed's tools**, so every write is attributable and shown in the chat.
- **Same device as the conductor**, as in the conductor design. Viewing memory from another device
  or the iPhone app is nice to have, not required.
- **Personal.** Nothing is committed to the user's repo. Teammates never see it.
- **No resold model access.** All model work runs on the user's own signed-in CLIs, as everywhere in
  Godspeed.

## Decided with the user

- Memory you can check comes before stuck detection.
- The conductor knows how long it's been away and re-checks against the latest code and context.
- It assumes others work on the repo too.
- The conductor is personal for now.
- Saving follows option C: facts are saved and shown with Undo, and guesses ask first.
- Older memories count for less.
- The main agent restarts on a fresh session, rebuilt from memory, after a real gap or when its
  context is nearly full (§9a).
- Zoomies and conductors share the new memory engine (§11).

## My calls (please check)

- Weight by "last confirmed," not "created," and fade at different speeds per kind.
- Ruled-out approaches reopen when the code they depended on changes.
- Creation runs the wake-up and replaces piece 3's accept-every-addition flow.
- Notes become the user's scratchpad, and the agent writes memory instead.
- Other people's content can create memory entries but never standing orders or permissions.
- One "while you were away" note, only after a real gap with real changes.
- Only the main agent writes memory. A worker's outcome is "checked" only with evidence the engine
  can see.
- Technical guesses are saved labelled, without asking. Only guesses about the user ask.
- Wake-up has two parts: a cheap engine check on each user message, then model re-checks in a
  separate job that hands the main chat a digest. Nothing runs while the user is away.
- Creation lands immediately and drafts as the first turn. A user's edit beats the draft.
- Entries the user stated or edited never expire on a timer.
- Off-device: "Remembered" lines are read-only, and guesses can be answered by replying.
- Zoomies: every run is a wake-up. Guesses queue in the inbox or Telegram. Existing `MEMORY.md`
  is imported as unconfirmed guesses. No memory is shared between Zoomies and conductors.
- Shipping order: shared engine, then conductors, then Zoomies.

## Left to the builder

- **Storage.** One store for conductor and Zoomie memory, keyed by owner. That could be a file per
  owner in the data dir (Zoomies use `agents/{id}/MEMORY.md` today), a small database, or a separate
  sync room. It has to meet the constraints above. A git-backed
  directory gets history for free.
- **The entry schema.** At minimum: text, kind, source (with a citation), seen or guess, created,
  last confirmed, and superseded-by.
- **The MCP tools** that replace or sit beside `conductor_notes_write`. Add, patch, confirm and
  retire make sense; whole-file replace doesn't. Hermes dropped "replace" because models used it to
  overwrite.
- **Fade windows, gap thresholds, the size of the loaded index,** and the memory cap.
- **Where the memory view lives** and how the inline "Remembered" line and guess questions render in
  GPUI.
- **How wake-up gets Godspeed chats in the folder** (the message index, `SearchMessages`, isn't yet
  exposed to agents) and **PR/CI state** (GitHub access may be limited, as on PR #44).

## How we'll know it works

- **Planted false memory.** Seed an entry the code contradicts. The conductor corrects it on next
  use (Copilot ran this test).
- **A simulated week away.** Commits by another author change a remembered fact and touch a
  ruled-out approach. On wake-up the conductor posts an accurate note, corrects the fact and reopens
  the approach.
- **Re-anchoring.** On a bug a long conductor chat has been stuck on, compare the conductor with a
  fresh chat given only the bug report. The conductor should be no slower.
- **The user rarely has to correct memory**, and guess questions are few enough that they get read.
- **Zoomie instruction change.** Edit a Zoomie's instructions between two scheduled runs. The next
  run follows the new instructions and says so. This is the Dots failure, turned into a test.
- **Stale Zoomie memory.** Change the repo so an imported `MEMORY.md` fact is wrong. The next run
  corrects it rather than acting on it.
