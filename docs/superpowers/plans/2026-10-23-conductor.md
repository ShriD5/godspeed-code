# Conductor (piece 1) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A user can create a conductor from a folder, talk to its one long-lived main chat, and have the main agent start, reuse and steer nested sub-agents that appear as live cards and raise questions through an "Answer" pop-up.

**Architecture:** A conductor is a new synced registry row kind (`conductors`) that names a main chat, a folder, the repo spaces found in it, an agent config and a notes string. Chat membership is derived by walking `parent_chat_id` up to a conductor's main chat, so no existing `Chat` literal changes. Nesting, depth cap, low-risk-only parent approvals and instruction injection are enforced in the engine and MCP server; the UI adds a sidebar section, a creation sheet, a card transcript row and an Answer pop-up.

**Tech Stack:** Rust workspace (`zeron-proto`, `zeron-doc`, `zeron-rpc`, `zeron-engine`, `zeron-mcp`, `zeron-ui`), GPUI, Loro registry rows.

**Spec:** `docs/superpowers/specs/2026-10-23-conductor-design.md`. Piece 1 only (rollover and learning are separate plans).

## Global Constraints

- Display name is "Conductor", defined once as `zeron_proto::conductor::DISPLAY_NAME`; stored and wire names use `conductor`.
- Depth cap: `MAX_CONDUCTOR_DEPTH = 3` levels below the main chat (main chat is depth 0).
- Access never widens going down: a child's access mode is capped by its parent's (existing rule, kept).
- Parents may answer **low-risk approvals only** (`ApprovalRisk::Low`); medium/high approvals and every question wait for the user.
- Notes are capped at `MAX_NOTES_BYTES = 32 * 1024`, same as always-on `MEMORY.md`.
- Chats outside a conductor behave exactly as before, including the one-level child rule.
- The existing word "project" (space) is untouched.
- Same device: wake-ups and sub-agents run on the device holding the folder.

## Review Focus

- Folder with no git repos: conductor still creates, with zero repos and a clear empty state.
- Folder that is itself a repo: exactly one repo, no duplicate spaces on a second conductor over the same folder.
- Two conductors over the same repo: spaces are reused, each chat belongs to exactly one conductor.
- Dangling `parent_chat_id` or a parent cycle in the registry: membership walk terminates and returns `None`.
- Archived or deleted main chat: conductor still lists and can be re-pointed or archived, not a crash.

---

## File Structure

```text
crates/proto/src/conductor.rs        # Conductor, ConductorSummary, params, depth consts, instructions text
crates/rpc/src/lib.rs                # method names (additive)
crates/doc/src/registry/conductors.rs# row kind, CRUD, membership walk
crates/engine/src/conductor.rs       # discovery + create/update/notes service on WorkspaceHost
crates/engine/src/rpc/conductor.rs   # RPC handlers
crates/engine/src/sessions.rs        # prompt prefix for conductor chats (small edit)
crates/engine/src/delegation.rs      # parent-eligibility swap (small edit)
crates/mcp/src/tools.rs              # nesting, conductor tools, low-risk gate
crates/ui/src/conductor/             # sidebar section, create sheet, card row, answer pop-up
docs/mcp.md, CONTEXT.md              # vocabulary and tool docs
```

### Task 1: Wire types and method names

**Files:**
- Create: `crates/proto/src/conductor.rs`; Modify: `crates/proto/src/lib.rs` (re-export), `crates/rpc/src/lib.rs`

**Interfaces:**
- Produces: `Conductor { id, name, purpose: Option<String>, folder, device_id, main_chat_id, space_ids: Vec<String>, config: Option<ChatConfig>, notes: String, archived: bool, created_at }`; `ConductorMembership { conductor_id, depth: u8 }`; `CreateConductorParams { folder, name?, purpose?, device_id?, config?, chat_id }`; `UpdateConductorParams { id, name?, purpose?, config?, archived? }`; `fn main_agent_instructions(&Conductor, repos: &[(String,String)]) -> String`; `fn sub_agent_instructions(&Conductor, depth: u8) -> String`; `fn can_spawn_at(depth: u8) -> bool`; methods `CREATE_CONDUCTOR`, `LIST_CONDUCTORS`, `UPDATE_CONDUCTOR`, `RESCAN_CONDUCTOR`, `GET_CONDUCTOR_NOTES`, `SET_CONDUCTOR_NOTES`, `GET_CONDUCTOR_TREE`.

- [ ] **Step 1: Write failing tests** in `conductor.rs`: `can_spawn_at(3)` is false and `can_spawn_at(2)` true; `main_agent_instructions` contains the purpose, every repo path and the notes; notes over `MAX_NOTES_BYTES` are truncated with a marker; `Conductor` JSON round-trips camelCase.
- [ ] **Step 2: Run** `cargo test -p zeron-proto conductor` → FAIL (module missing).
- [ ] **Step 3: Implement** the types above; instructions text states: report short, say where work was sent, read but delegate changes, reuse sub-agents by title, low-risk approvals only, nesting allowed to depth 3.
- [ ] **Step 4: Run** the same command → PASS.
- [ ] **Step 5: Commit** `feat(proto): conductor wire types`.

### Task 2: Registry rows and membership

**Files:**
- Create: `crates/doc/src/registry/conductors.rs`; Modify: `crates/doc/src/registry.rs` (`KIND_CONDUCTORS`, `mod conductors;`), `crates/doc/src/registry/tests.rs`

**Interfaces:**
- Consumes: Task 1 `Conductor`.
- Produces on `RegistryDoc`: `upsert_conductor(&mut self, &Conductor) -> Result<(), DocError>`, `update_conductor(&mut self, id, ConductorPatch) -> Result<bool, DocError>`, `conductors(&self) -> Vec<Conductor>`, `conductor(&self, id) -> Option<Conductor>`, `conductor_of_chat(&self, chat_id: &str) -> Option<ConductorMembership>` (walks parents, max 16 hops, cycle-safe).

- [ ] **Step 1: Write failing tests** `conductor_row_round_trips`, `membership_depth_counts_from_main_chat`, `membership_survives_dangling_parent_as_none`, `membership_terminates_on_parent_cycle`, `notes_update_is_last_writer_wins`.
- [ ] **Step 2: Run** `cargo test -p zeron-doc conductor` → FAIL.
- [ ] **Step 3: Implement** with `write(KIND_CONDUCTORS, id, Upsert|Update, fields(..))` and `overlay_rows`, modelled on `sidebar_pins.rs`; main chat itself is depth 0.
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat(doc): conductor registry rows and membership`.

### Task 3: Repo discovery

**Files:**
- Create: `crates/engine/src/conductor/discover.rs` (declared from `conductor.rs`); Modify: `crates/engine/src/lib.rs` (`pub mod conductor;`)

**Interfaces:**
- Produces: `fn discover_repos(folder: &Path) -> Vec<PathBuf>`: the folder itself if it contains `.git`, else its direct children containing `.git`, sorted, skipping hidden dirs and symlinks.

- [ ] **Step 1: Write failing tests** with `tempfile`: folder-is-repo → `[folder]`; parent with 2 repos and 1 plain dir → 2 sorted repos; empty folder → `[]`; hidden `.cache/.git` ignored; a file named `.git` (worktree) counts.
- [ ] **Step 2: Run** `cargo test -p zeron-engine conductor::discover` → FAIL.
- [ ] **Step 3: Implement** with `std::fs::read_dir`, no recursion beyond one level.
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat(engine): discover repos inside a conductor folder`.

### Task 4: Conductor service on the workspace host

**Files:**
- Modify: `crates/engine/src/conductor.rs`, `crates/engine/src/workspace_host.rs` (thin delegating methods)

**Interfaces:**
- Consumes: Tasks 1–3, `WorkspaceHost::create_space`, `create_chat_with_parent_notify`.
- Produces on `WorkspaceHost`: `create_conductor(&self, CreateConductorParams) -> Result<Conductor, EngineError>` (reuses an existing space for the same `(device, path)`, creates the main chat with the conductor's config at the folder cwd, writes the row); `update_conductor`; `rescan_conductor(&self, id) -> Result<Conductor, EngineError>` (adds new repo spaces, never removes); `set_conductor_notes(&self, id, &str) -> Result<(), EngineError>` (rejects > `MAX_NOTES_BYTES`); `conductor_tree(&self, id) -> Result<Vec<ConductorNode>, EngineError>` where `ConductorNode { chat_id, parent_chat_id, depth, title, space_id }`; `conductor_membership(&self, chat_id) -> Result<Option<ConductorMembership>, EngineError>`; `chat_may_parent(&self, &Chat) -> Result<bool, EngineError>` = top-level or member with `can_spawn_at(depth)`.
- Also: in `create_chat_with_parent_notify`, replace both `parent.is_top_level()` requirements with `chat_may_parent`; reject children deeper than the cap.

- [ ] **Step 1: Write failing tests** (using the existing `WorkspaceHost` test constructor): create over a two-repo temp folder yields 2 spaces and a main chat; second conductor over same folder reuses both spaces; nested chat at depth 3 is created and depth 4 is rejected with a message naming the cap; a non-conductor child still cannot parent (old rule); notes over cap rejected; rescan picks up a newly added repo.
- [ ] **Step 2: Run** `cargo test -p zeron-engine conductor` → FAIL.
- [ ] **Step 3: Implement** as above.
- [ ] **Step 4: Run** → PASS, plus `cargo test -p zeron-engine delegation` still PASS.
- [ ] **Step 5: Commit** `feat(engine): conductor service with nesting and depth cap`.

### Task 5: Delegation wake-ups to any eligible parent

**Files:**
- Modify: `crates/engine/src/delegation.rs` (the two `parent.is_top_level()` checks at ~320 and ~419)

**Interfaces:**
- Consumes: Task 4 `chat_may_parent`.

- [ ] **Step 1: Write failing test** in `delegation.rs` tests: a child of a depth-1 conductor member with `notifyParent` produces a notice for its direct parent; a child of a non-conductor child still produces none.
- [ ] **Step 2: Run** `cargo test -p zeron-engine delegation` → FAIL.
- [ ] **Step 3: Implement**: replace `!parent.is_top_level()` with `!workspace.chat_may_parent(&parent)?` (eligibility only; wake target stays the direct parent).
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat(engine): wake nested conductor parents`.

### Task 6: RPC handlers

**Files:**
- Create: `crates/engine/src/rpc/conductor.rs`; Modify: `crates/engine/src/rpc.rs` (`mod conductor;`, `forwardable`, dispatch)

**Interfaces:**
- Consumes: Task 4. Produces: handlers for the Task 1 methods returning `Conductor`, `Vec<Conductor>`, `{notes}`, `Vec<ConductorNode>`.

- [ ] **Step 1: Write failing test** in `rpc.rs` tests: `CreateConductor` over a temp folder then `ListConductors` returns it; `SetConductorNotes` then `GetConductorNotes` round-trips; unknown id errors.
- [ ] **Step 2: Run** `cargo test -p zeron-engine conductor_rpc` → FAIL.
- [ ] **Step 3: Implement** `handles(method)` and `conductor_rpc`, modelled on `limit_recovery.rs`; mark forwardable.
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat(engine): conductor RPCs`.

### Task 7: Instruction injection

**Files:**
- Modify: `crates/engine/src/sessions.rs` (next to the `carried_context` block, ~2561)

**Interfaces:**
- Consumes: `conductor_membership`, `main_agent_instructions`, `sub_agent_instructions`.

- [ ] **Step 1: Write failing test** for a pure helper `conductor_prompt_prefix(ws: &WorkspaceHost, chat_id) -> Option<String>`: main chat gets main instructions plus notes; depth-1 chat gets sub-agent instructions plus notes; unrelated chat gets `None`.
- [ ] **Step 2: Run** `cargo test -p zeron-engine conductor_prompt` → FAIL.
- [ ] **Step 3: Implement** the helper; in `sessions.rs`, when `request.resume.is_none() && !resume_state.idle`, prepend the prefix to `request.prompt` (before carried context).
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat(engine): inject conductor instructions and notes`.

### Task 8: MCP tools and approval gate

**Files:**
- Modify: `crates/mcp/src/tools.rs`, `crates/mcp/src/godspeed.rs` (RPC helpers), `docs/mcp.md`

**Interfaces:**
- Produces tools: `conductor_info` → `{conductor, repos:[{space,path,name}], subAgents:[{chatId,title,depth,parentChatId,status}]}`; `conductor_notes_read`; `conductor_notes_write {notes}`.
- Changes: `create_chat` accepts a parent that is a conductor member below the cap (replaces the "Cannot create a child of a side chat" and "Side chats cannot create chats" checks via an engine membership RPC); `respond_to_input` for a conductor origin approves only `ApprovalRisk::Low` (`approval_risk(approval, chat.cwd)`), always allows declining.

- [ ] **Step 1: Write failing tests** next to the existing `orchestrators_approve_only_within_their_own_access_mode`: conductor main agent approving a medium-risk approval is refused with a message naming the risk; low-risk allowed; decline allowed; nested creation allowed at depth 2, refused at 3 with the cap in the message; non-conductor side chat still refused.
- [ ] **Step 2: Run** `cargo test -p zeron-mcp conductor` → FAIL.
- [ ] **Step 3: Implement** the three tools, the membership-aware parent checks, the low-risk gate; document in `docs/mcp.md`.
- [ ] **Step 4: Run** `cargo test -p zeron-mcp` → PASS.
- [ ] **Step 5: Commit** `feat(mcp): conductor tools, nesting and low-risk approval gate`.

### Task 9: Attention link to the top-level card

**Files:**
- Modify: `crates/proto/src/attention.rs` (additive field), `crates/engine/src/attention/derive.rs`

**Interfaces:**
- Produces: `AttentionItem.conductor: Option<ConductorLink { conductor_id, card_chat_id }>` where `card_chat_id` is the depth-1 ancestor, so the UI lights the right card.

- [ ] **Step 1: Write failing test** in `derive.rs`: a pending approval in a depth-2 chat carries `card_chat_id` of its depth-1 ancestor; a non-member item has `None`.
- [ ] **Step 2: Run** `cargo test -p zeron-engine attention` → FAIL.
- [ ] **Step 3: Implement** using `conductor_of_chat` and a parent walk; serde `default`, `skip_serializing_if`.
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat(attention): link nested items to their conductor card`.

### Task 10: UI

**Files:**
- Create: `crates/ui/src/conductor/{mod.rs,sidebar.rs,create_sheet.rs,card.rs,answer_popup.rs}`; Modify: sidebar and transcript hookups, command palette entry, `crates/ui/src/approval_view.rs` (reuse only)

**Interfaces:**
- Consumes: RPCs from Task 6, `AttentionItem.conductor` from Task 9, `approval_view::approval_details`.

- [ ] **Step 1: Write failing tests** for pure view-model functions in `conductor/mod.rs`: `card_model(chat, session, attention) -> CardModel` (status, last three steps from `subagent_tail`-style tail, nested count, `needs_you`); `cards_for_main_chat(registry) -> Vec<CardModel>` includes only depth-1 children; `conductor_needs_you(...)` true when any descendant waits.
- [ ] **Step 2: Run** `cargo test -p zeron-ui conductor` → FAIL.
- [ ] **Step 3: Implement** the view models, then the sidebar "Conductors" section (entry opens the main chat tab, amber dot on needs-you), the creation sheet (folder picker, optional name and purpose, agent/model/access, calls `CREATE_CONDUCTOR`), the card row (amber state and "Answer" button, "Open thread"), and the Answer pop-up reusing `approval_details` and a minimal option list for questions. Also a "New conductor" command-palette action.
- [ ] **Step 4: Run** `cargo test -p zeron-ui conductor` and `cargo check -p godspeed` → PASS.
- [ ] **Step 5: Commit** `feat(ui): conductor sidebar, creation sheet, cards and answer pop-up`.

### Task 11: Docs and workspace check

**Files:**
- Modify: `CONTEXT.md` (Conductor vocabulary), `docs/superpowers/specs/2026-10-23-conductor-design.md` (notes live on the row), `CHANGELOG.md`

- [ ] **Step 1:** Add the vocabulary and the notes-storage call to the spec.
- [ ] **Step 2: Run** `cargo fmt --all -- --check`, `cargo clippy -p zeron-proto -p zeron-doc -p zeron-engine -p zeron-mcp -p zeron-ui -- -D warnings`, `cargo test -p zeron-proto -p zeron-doc -p zeron-engine -p zeron-mcp` → all PASS.
- [ ] **Step 3: Commit** `docs: conductor vocabulary and notes storage`.

---

## Self-review

- **Spec coverage:** creation (T3,T4,T6,T10), main agent abilities (T8), nesting and cap (T4,T5,T8), cards and Answer pop-up (T9,T10), who may answer (T8), notes (T1,T4,T7,T8), changing the main agent (`UpdateConductor` config in T4/T6; the rollover that makes it seamless is piece 2, so until then it applies from the next fresh session, as the spec says), naming (T1 constant). Rollover and learning are intentionally out of this plan.
- **Deviation from spec, flagged:** notes live on the synced conductor row, not a file, so they sync and need no data-dir plumbing.
- **Access clamp on lowering the main agent's access** needs rollover-style restarts; this plan records the new config and relies on the existing child-capped-by-parent rule at spawn time. Live clamping is deferred to piece 2.
