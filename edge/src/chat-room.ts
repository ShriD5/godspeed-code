/**
 * ChatRoom — one Durable Object per chat session (`chat2/{chatId}`), the
 * dumb authenticated log relay replacing SessionRoom's loro-aware s2 rooms
 * (docs/chat2-sync.md workstream B). Modeled line-for-line on RegistryRoom,
 * NOT on SessionRoom: no loro-wasm import anywhere in this class.
 *
 * The DO's entire job: append opaque update blobs to a seq-ordered log,
 * relay them to live sockets, store one client-built checkpoint blob, and
 * serve both back. All CRDT semantics live in the clients. Cold start is a
 * table read — the s2 wedge class (CPU-limited export/replay in the DO)
 * cannot exist here by construction.
 *
 * Sidecars (`/tail`, `/diff`) are host-PUBLISHED and served verbatim: the DO
 * never materializes anything.
 *
 * Write budget: the free plan allows 100k SQLite rows written per day for
 * the whole account, and a streaming turn pushes several updates a second.
 * Pushes are coalesced: the first after a quiet second is written at once,
 * later ones wait (at most `FLUSH_INTERVAL_MS`) and land together as one
 * chunk row. A push is acked and relayed only after its chunk commits, so an
 * ack still means durable; a crash before the flush loses only unacked
 * batches, which clients keep in their outbox and replay on reconnect (the
 * batch-id dedupe makes the replay exact-once).
 *
 * Hibernation discipline: no timers while idle (the flush timer only runs
 * while pushes are buffered); ping/pong rides the auto-response pair; alarms
 * retry accepted rows' host wakes and back up to R2.
 */
import { createBlobStore, type BlobStore } from "./blobs";
import {
  appendRows,
  batchIndex,
  CHECKPOINT_BLOB,
  commitCheckpoint,
  ensureChatLog,
  FRONTIER_BLOB,
  getMeta,
  headSeq,
  logStats,
  MAX_ROW_BYTES,
  rowError,
  rowsAfter,
  setMeta,
  type NewRow
} from "./chat-log";
import { decodeFrame, encodeFrame, FRAME } from "./chat-frames";
import { AUTH_USER_HEADER, type Env } from "./env";
import {
  ensureChatWakes, finishChatWake, MAX_WAKE_ATTEMPTS, PERMANENT_WAKE_REJECTIONS,
  pendingChatWake, queueChatWake, retryChatWake, validWakeRoute
} from "./chat-wakes";
import { PushOutcomes } from "./push-outcomes";
import { WriteMeter } from "./write-meter";

const DAY_MS = 24 * 60 * 60 * 1000;
/** Inbound frame budget: one pushed row (+ header slack). */
const MAX_FRAME_BYTES = MAX_ROW_BYTES + 8192;
/** Host-published sidecar budget (tail JSON / diff payload). */
const MAX_SIDECAR_BYTES = 4 * 1024 * 1024;
/** Existing long-running sessions exceed 16 MiB. Keep uploads bounded,
 * but allow their checkpoints to advance instead of stranding the row log. */
export const MAX_CHECKPOINT_BYTES = 32 * 1024 * 1024;
/** Presence beats older than this are swept before relay/stats. */
const PRESENCE_TTL_MS = 30_000;
/** Per-device push quota, rolling window (in-memory; resets on hibernation —
 * it exists to contain a runaway client loop, not to meter honest traffic). */
const QUOTA_WINDOW_MS = 60_000;
const QUOTA_MAX_PUSHES = 300;
const QUOTA_MAX_BYTES = 8 * 1024 * 1024;
/** Streaming coalescing window: at most one chunk write per window. */
export const FLUSH_INTERVAL_MS = 1000;
/** Buffered payload that forces an early flush. */
export const FLUSH_BYTES = 256 * 1024;

interface SocketState {
  userId: string;
  device: string;
  /** Set once a valid hello established the session. */
  ready?: boolean;
  chatId?: string;
  hostDevice?: string;
}

interface QuotaWindow {
  since: number;
  pushes: number;
  bytes: number;
}

/** Who is owed the outcome of a buffered push. */
type FlushOutcome = { ok: true; seq: number; dup: boolean } | { ok: false };

interface Buffered {
  row: NewRow;
  userId: string;
  chatId: string;
  hostDevice: string;
  /** Sockets owed an ack: the pusher, plus any replay of the same batch. */
  sockets: Array<{ ws: WebSocket; dup: boolean }>;
  /** HTTP pushers awaiting the outcome. */
  waiters: Array<{ resolve: (outcome: FlushOutcome) => void; dup: boolean }>;
}

export class ChatRoom implements DurableObject {
  private readonly ctx: DurableObjectState;
  private readonly env: Env;
  private readonly blobs: BlobStore;
  /** The room's SQL, metered (rows written per minute show in the logs). */
  private readonly sql: SqlStorage;
  readonly meter: WriteMeter;
  private readonly outcomes: PushOutcomes;
  /** Accepted pushes not yet written (nor acked, nor relayed). */
  private buffer: Buffered[] = [];
  private bufferBytes = 0;
  private lastFlushAt = 0;
  private flushTimer?: ReturnType<typeof setTimeout>;
  private flushChain: Promise<void> = Promise.resolve();
  /** Batch-id dedupe set, valid while the head it was built at holds. */
  private known?: { head: number; index: Map<string, number> };
  /** Coalescing window (tests shorten it). */
  flushIntervalMs = FLUSH_INTERVAL_MS;
  /** device → last presence beat (epoch ms). Memory-only by construction. */
  private readonly presence = new Map<string, number>();
  /** device → rolling push quota window. Memory-only. */
  private readonly quotas = new Map<string, QuotaWindow>();
  private wakeFlight?: Promise<void>;

  constructor(ctx: DurableObjectState, env: Env) {
    this.ctx = ctx;
    this.env = env;
    this.meter = new WriteMeter(`chat2 ${ctx.id.toString().slice(0, 12)}`);
    this.sql = this.meter.wrap(ctx.storage.sql);
    ensureChatLog(this.sql);
    ensureChatWakes(this.sql);
    this.blobs = createBlobStore(this.sql);
    this.outcomes = new PushOutcomes((key) => getMeta(this.sql, key), (key, value) => setMeta(this.sql, key, value));
    // Runtime-answered keepalive; proves nothing about this DO's health.
    // Clients judge liveness by probe frames (same caveat as RegistryRoom).
    ctx.setWebSocketAutoResponse(new WebSocketRequestResponsePair("ping", "pong"));
  }

  // ── HTTP surface (only reachable through the authed Worker) ──────────────

  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    const userId = request.headers.get(AUTH_USER_HEADER);
    if (!userId) return json({ error: "unauthenticated" }, 401);

    const sql = this.sql;
    const owner = getMeta(sql, "owner");

    if (url.pathname === "/ws") {
      // Claim-on-first-join ownership, then owner-only forever (the s2
      // discipline: chat ids are client-minted, the first authed user to
      // dial one owns it).
      if (!owner) setMeta(sql, "owner", userId);
      else if (owner !== userId) return json({ error: "forbidden" }, 403);
      const device = url.searchParams.get("device") ?? "";
      const chatId = url.searchParams.get("chatId") ?? "";
      // An unroutable wake hint never refuses the push: the rows are the
      // user's message; only host discovery is skipped.
      const hint = url.searchParams.get("hostDevice") ?? "";
      const hostDevice = hint && validWakeRoute(chatId, hint) ? hint : "";
      const pair = new WebSocketPair();
      this.ctx.acceptWebSocket(pair[1]);
      const state: SocketState = { userId, device, chatId, hostDevice };
      pair[1].serializeAttachment(state);
      return new Response(null, { status: 101, webSocket: pair[0] });
    }

    if (url.pathname === "/checkpoint" && request.method === "POST") {
      if (!owner) setMeta(sql, "owner", userId);
      else if (owner !== userId) return json({ error: "forbidden" }, 403);
      const seqCovered = Number(url.searchParams.get("seqCovered") ?? "");
      if (!Number.isInteger(seqCovered) || seqCovered < 0) {
        return json({ error: "bad_seq_covered" }, 400);
      }
      const frontier = decodeBase64(request.headers.get("x-chat2-frontier") ?? "");
      if (frontier === undefined) return json({ error: "bad_frontier" }, 400);
      // A checkpoint that claims to cover rows must name its state: an empty
      // frontier label on a content-bearing checkpoint made every fresh
      // reader skip it and park all dependent rows invisibly ("Add Tweets"
      // incident, 2026-08-18). Empty stays legal only for seqCovered 0
      // (the M1 empty-doc seed).
      if (frontier.byteLength === 0 && seqCovered > 0) {
        return json({ error: "bad_frontier", message: "empty frontier on a content checkpoint" }, 400);
      }
      const body = new Uint8Array(await request.arrayBuffer());
      if (body.byteLength > MAX_CHECKPOINT_BYTES) return json({ error: "too_large" }, 413);
      await this.flush();
      const outcome = commitCheckpoint(sql, this.blobs, seqCovered, frontier, body, Date.now());
      this.known = undefined;
      if (!outcome.ok) return json({ error: outcome.error }, 409);
      await this.markBackupDirty();
      return json({ ok: true, seqFloor: outcome.seqFloor, pruned: outcome.pruned });
    }

    // Claim-on-first-contact for the HTTP surface too — same client-minted-id
    // discipline as /ws. The /rows twins predate the pull-first HTTPS
    // transport and 404'd an unclaimed room, which deadlocked a brand-new
    // chat on WS-hostile networks: the sender's push 404s, the host's pull
    // 404s, and the only claimants (WS join, checkpoint heal) never run.
    if (!owner) setMeta(sql, "owner", userId);
    else if (owner !== userId) return json({ error: "forbidden" }, 403);

    if (url.pathname === "/checkpoint" && request.method === "GET") {
      const bytes = this.blobs.get(CHECKPOINT_BLOB);
      if (!bytes) return json({ error: "not_found" }, 404);
      // Range-resumable (bytes=N- only): a 1MB load over a 1.2Mbps link that
      // dies at byte 800k resumes, where s2's export-per-join restarted.
      const range = parseRangeStart(request.headers.get("range"));
      if (range !== null && range >= bytes.byteLength) {
        return new Response(null, {
          status: 416,
          headers: { "content-range": `bytes */${bytes.byteLength}` }
        });
      }
      const body = range !== null ? bytes.subarray(range) : bytes;
      const headers = new Headers({
        "content-type": "application/octet-stream",
        "content-length": String(body.byteLength),
        "accept-ranges": "bytes",
        "x-chat2-checkpoint-seq": getMeta(sql, "checkpointSeq") ?? "0"
      });
      if (range !== null) {
        headers.set(
          "content-range",
          `bytes ${range}-${bytes.byteLength - 1}/${bytes.byteLength}`
        );
      }
      return new Response(body, { status: range !== null ? 206 : 200, headers });
    }

    if (url.pathname === "/rows" && request.method === "GET") {
      // Pull over plain HTTPS: one GET collapses connect → hello → state →
      // rowsReq → backfill (4+ serial round trips on a WS, and impossible on
      // networks that strip the upgrade) into a single request. The body is
      // u32-LE length-prefixed frames — state (frontier payload), rows after
      // `?after=`, rowsDone — byte-identical frame encoding to the WS path,
      // so clients reuse their existing decoder.
      const afterRaw = Number(url.searchParams.get("after") ?? "0");
      const after = Number.isInteger(afterRaw) && afterRaw >= 0 ? afterRaw : 0;
      const device = url.searchParams.get("device") ?? "";
      const exclude =
        url.searchParams.get("excludeOwn") === "1" && device !== "" ? device : undefined;
      const stats = logStats(sql);
      const frontier = this.blobs.get(FRONTIER_BLOB) ?? new Uint8Array(0);
      const frames: Uint8Array[] = [
        encodeFrame(
          FRAME.state,
          {
            headSeq: stats.headSeq,
            seqFloor: stats.seqFloor,
            checkpointSeq: stats.checkpointSeq,
            checkpointSize: stats.checkpointSize,
            rowCount: stats.rowCount,
            rowBytes: stats.rowBytes
          },
          frontier
        )
      ];
      // Response cap: the WS path streams; this buffers, so bound the body.
      // Past the cap the response ends WITHOUT rowsDone — clients apply what
      // arrived, their cursor advances per row, and the next pull resumes
      // from there (pagination by truncation).
      const ROWS_BODY_CAP = 4 * 1024 * 1024;
      let bodyBytes = 0;
      let truncated = false;
      for (const row of rowsAfter(sql, after, exclude)) {
        const frame = encodeFrame(
          FRAME.row,
          { seq: row.seq, device: row.device, batchId: row.batchId },
          row.bytes
        );
        bodyBytes += 4 + frame.length;
        if (bodyBytes > ROWS_BODY_CAP) {
          truncated = true;
          break;
        }
        frames.push(frame);
      }
      if (!truncated) {
        frames.push(encodeFrame(FRAME.rowsDone, { headSeq: headSeq(sql) }));
      }
      const total = frames.reduce((n, f) => n + 4 + f.length, 0);
      const body = new Uint8Array(total);
      const view = new DataView(body.buffer);
      let off = 0;
      for (const f of frames) {
        view.setUint32(off, f.length, true);
        body.set(f, off + 4);
        off += 4 + f.length;
      }
      return new Response(body, {
        headers: { "content-type": "application/octet-stream" }
      });
    }

    if (url.pathname === "/rows" && request.method === "POST") {
      // Push over plain HTTPS — the WS push's fallback twin for networks
      // where the upgrade never completes. batchId dedupe (UNIQUE column)
      // makes at-least-once delivery exact-once in effect.
      const device = url.searchParams.get("device") ?? "";
      const batchId = url.searchParams.get("batchId") ?? "";
      const chatId = url.searchParams.get("chatId") ?? "";
      const hint = url.searchParams.get("hostDevice") ?? "";
      const hostDevice = hint && validWakeRoute(chatId, hint) ? hint : "";
      if (batchId === "" || batchId.length > 128) {
        this.recordPush(device, false);
        return json({ error: "bad_push" }, 400);
      }
      // Pre-read cap (the WS runtime closes 1 MiB messages before the DO
      // runs; HTTP needs the explicit twin). appendRow re-checks post-read.
      const declared = Number(request.headers.get("content-length") ?? "0");
      if (declared > MAX_ROW_BYTES + 4096) {
        this.recordPush(device, false);
        return json({ error: "too_large" }, 413);
      }
      const payload = new Uint8Array(await request.arrayBuffer());
      if (!this.admitQuota(device, payload.byteLength)) {
        this.recordPush(device, false);
        return json({ error: "quota" }, 429);
      }
      const error = rowError(payload);
      if (error) {
        this.recordPush(device, false);
        return json({ error }, error === "too_large" ? 413 : 400);
      }
      // HTTPS pushes flush at once (no coalescing wait): the fallback
      // transport drains its outbox one request at a time. The flush relays
      // to every ready socket — a same-device socket would re-import its own
      // bytes as a Loro no-op, so no exclusion needed.
      const outcome = await new Promise<FlushOutcome>((resolve) => {
        this.enqueue({ device, batchId, bytes: payload }, userId, chatId, hostDevice, { resolve });
        void this.flush();
      });
      if (!outcome.ok) return json({ error: "unavailable" }, 503);
      this.recordPush(device, true);
      return json({ batchId, seq: outcome.seq, dup: outcome.dup });
    }

    if ((url.pathname === "/tail" || url.pathname === "/diff") && request.method === "PUT") {
      const name = url.pathname === "/tail" ? "sidecar-tail" : "sidecar-diff";
      const body = new Uint8Array(await request.arrayBuffer());
      if (body.byteLength > MAX_SIDECAR_BYTES) return json({ error: "too_large" }, 413);
      this.blobs.put(name, body);
      setMeta(sql, `${name}-type`, request.headers.get("content-type") ?? "application/json");
      return json({ ok: true, bytes: body.byteLength });
    }

    if ((url.pathname === "/tail" || url.pathname === "/diff") && request.method === "GET") {
      const name = url.pathname === "/tail" ? "sidecar-tail" : "sidecar-diff";
      const bytes = this.blobs.get(name);
      if (!bytes) return json({ error: "not_found" }, 404);
      return new Response(bytes, {
        headers: {
          "content-type": getMeta(sql, `${name}-type`) ?? "application/json",
          "content-length": String(bytes.byteLength)
        }
      });
    }

    if (url.pathname === "/stats" && request.method === "GET") {
      this.sweepPresence();
      return json({
        ...logStats(sql),
        connectedSockets: this.ctx.getWebSockets().length,
        presence: Object.fromEntries(this.presence),
        // The ONLY per-device attribution surface — kept from the 2026-08-05
        // incident tooling (SessionRoom's /stats pushOutcomes).
        pushOutcomes: this.outcomes.snapshot(),
        bufferedRows: this.buffer.length,
        lastBackupSeq: Number(getMeta(sql, "backupSeq") ?? "0")
      });
    }

    if (url.pathname === "/reset" && request.method === "POST") {
      // Operator wipe. Recovery is host-driven: the host detects
      // `headSeq < cursor` on its next hello and re-seeds via checkpoint —
      // same shape as the registry reset recipe.
      await this.flush();
      sql.exec("DELETE FROM rows");
      sql.exec("DELETE FROM chunks");
      sql.exec("DELETE FROM meta");
      sql.exec("DELETE FROM blobs");
      sql.exec("DELETE FROM chat_wake");
      this.known = undefined;
      this.outcomes.reset();
      for (const ws of this.ctx.getWebSockets()) {
        try {
          ws.close(4410, "chat room reset");
        } catch {
          /* already gone */
        }
      }
      return json({ ok: true });
    }

    return json({ error: "not found" }, 404);
  }

  // ── WebSocket protocol (binary frames, chat-frames.ts) ───────────────────

  async webSocketMessage(ws: WebSocket, message: ArrayBuffer | string): Promise<void> {
    if (typeof message === "string") {
      ws.close(1003, "binary frames only");
      return;
    }
    if (message.byteLength > MAX_FRAME_BYTES) {
      ws.close(1009, "frame too large");
      return;
    }
    const frame = decodeFrame(new Uint8Array(message));
    const state = ws.deserializeAttachment() as SocketState;
    if (!frame) {
      send(ws, FRAME.error, { code: "bad_frame", message: "malformed frame" });
      return;
    }
    switch (frame.type) {
      case FRAME.hello:
        this.handleHello(ws, state, frame.header);
        return;
      case FRAME.rowsReq:
        this.handleRowsReq(ws, state, frame.header);
        return;
      case FRAME.push:
        await this.handlePush(ws, state, frame.header, frame.payload);
        return;
      case FRAME.presence:
        this.handlePresence(ws, state, frame.header, frame.payload);
        return;
      case FRAME.probe:
        send(ws, FRAME.probeOk, { headSeq: headSeq(this.sql) });
        return;
      default:
        send(ws, FRAME.error, { code: "bad_frame", message: `unexpected type ${frame.type}` });
    }
  }

  /** A closing socket's buffered pushes still commit (and relay); their
   * acks are lost with it, so the client replays them — deduped. */
  async webSocketClose(): Promise<void> {
    await this.flush();
  }

  async webSocketError(): Promise<void> {
    await this.flush();
  }

  private handleHello(ws: WebSocket, state: SocketState, header: Record<string, unknown>): void {
    if (typeof header.device === "string" && header.device.length > 0) {
      state.device = header.device;
    }
    state.ready = true;
    ws.serializeAttachment(state);
    const sql = this.sql;
    const stats = logStats(sql);
    const frontier = this.blobs.get(FRONTIER_BLOB) ?? new Uint8Array(0);
    // Metadata + frontier only — the CLIENT decides what to load next
    // (frontier included in local doc → rows only; not included → GET
    // /checkpoint first; cursor > headSeq → server lost state, re-seed).
    send(
      ws,
      FRAME.state,
      {
        headSeq: stats.headSeq,
        seqFloor: stats.seqFloor,
        checkpointSeq: stats.checkpointSeq,
        checkpointSize: stats.checkpointSize,
        rowCount: stats.rowCount,
        rowBytes: stats.rowBytes
      },
      frontier
    );
  }

  private handleRowsReq(ws: WebSocket, state: SocketState, header: Record<string, unknown>): void {
    if (!state.ready) {
      send(ws, FRAME.error, { code: "hello_first", message: "rows before hello" });
      return;
    }
    const after = typeof header.after === "number" && header.after >= 0 ? header.after : 0;
    const exclude = header.excludeOwn === true ? state.device : undefined;
    const sql = this.sql;
    for (const row of rowsAfter(sql, after, exclude)) {
      send(ws, FRAME.row, { seq: row.seq, device: row.device, batchId: row.batchId }, row.bytes);
    }
    send(ws, FRAME.rowsDone, { headSeq: headSeq(sql) });
  }

  private async handlePush(
    ws: WebSocket,
    state: SocketState,
    header: Record<string, unknown>,
    payload: Uint8Array
  ): Promise<void> {
    const batchId = typeof header.batchId === "string" ? header.batchId : "";
    // Push errors carry the batchId so clients can RETIRE permanently
    // rejected batches from their replay queues (an unretireable batch
    // replays on every reconnect forever — the wedge class this replaces).
    if (!state.ready || batchId === "" || batchId.length > 128) {
      this.recordPush(state.device, false);
      send(ws, FRAME.error, { code: "bad_push", message: "hello first / malformed push", batchId });
      return;
    }
    if (!this.admitQuota(state.device, payload.byteLength)) {
      this.recordPush(state.device, false);
      send(ws, FRAME.error, { code: "quota", message: "per-device push quota exceeded", batchId });
      return;
    }
    const error = rowError(payload);
    if (error) {
      this.recordPush(state.device, false);
      send(ws, FRAME.error, { code: error, message: `push rejected: ${error}`, batchId });
      return;
    }
    this.recordPush(state.device, true);
    // Relay to the OTHER ready sockets and the ack both wait for the flush
    // (contrast RegistryRoom, whose LWW merge means the sender must see the
    // merged truth — here bytes are opaque and Loro convergence is the
    // client's business).
    this.enqueue(
      { device: state.device, batchId, bytes: payload },
      state.userId,
      state.chatId ?? "",
      state.hostDevice ?? "",
      { ws }
    );
    // A push that wakes the host (a user's message from another device) never
    // waits out the window.
    const wakes = (state.hostDevice ?? "") !== "" && state.hostDevice !== state.device;
    const now = Date.now();
    if (wakes || this.bufferBytes >= FLUSH_BYTES || now - this.lastFlushAt >= this.flushIntervalMs) {
      await this.flush();
    } else if (!this.flushTimer) {
      this.flushTimer = setTimeout(() => {
        this.flushTimer = undefined;
        void this.flush();
      }, this.lastFlushAt + this.flushIntervalMs - now);
    }
  }

  /** Buffer one accepted push; a replay of a still-buffered batch joins it. */
  private enqueue(
    row: NewRow,
    userId: string,
    chatId: string,
    hostDevice: string,
    owed: { ws: WebSocket } | { resolve: (outcome: FlushOutcome) => void }
  ): void {
    let entry = this.buffer.find((b) => b.row.batchId === row.batchId);
    const dup = entry !== undefined;
    if (!entry) {
      entry = { row, userId, chatId, hostDevice, sockets: [], waiters: [] };
      this.buffer.push(entry);
      this.bufferBytes += row.bytes.byteLength;
    } else if (hostDevice && hostDevice !== row.device) {
      // A replay can still carry the wake the original lacked.
      Object.assign(entry, { userId, chatId, hostDevice });
    }
    if ("ws" in owed) entry.sockets.push({ ws: owed.ws, dup });
    else entry.waiters.push({ resolve: owed.resolve, dup });
  }

  /** Write everything buffered (serialized: acks and relays leave in seq
   * order even when flushes overlap). Never rejects. */
  private flush(): Promise<void> {
    if (this.flushTimer) {
      clearTimeout(this.flushTimer);
      this.flushTimer = undefined;
    }
    const run = this.flushChain.then(() => this.flushOnce());
    this.flushChain = run.catch(() => {});
    return run;
  }

  /** Buffered rows, their wake receipt, and the alarm are one SQLite storage
   * transaction; acks go out after it, so they cannot outrun durability
   * (the output gate holds them until the write is confirmed anyway). */
  private async flushOnce(): Promise<void> {
    const batch = this.buffer;
    this.buffer = [];
    this.bufferBytes = 0;
    if (batch.length === 0) return;
    this.lastFlushAt = Date.now();
    const sql = this.sql;
    const outcomes: Array<{ seq: number; dup: boolean }> = [];
    let known: { head: number; index: Map<string, number> } | undefined;
    const wake = [...batch].reverse().find((b) => b.hostDevice && b.hostDevice !== b.row.device);
    try {
      await this.ctx.storage.transaction(async () => {
        const head = headSeq(sql);
        const index = this.known?.head === head ? this.known.index : batchIndex(sql);
        this.known = undefined; // mutated below; reinstated on commit
        const fresh: NewRow[] = [];
        let next = head + 1;
        for (const entry of batch) {
          const existing = index.get(entry.row.batchId);
          if (existing !== undefined) {
            outcomes.push({ seq: existing, dup: true });
            continue;
          }
          index.set(entry.row.batchId, next);
          outcomes.push({ seq: next, dup: false });
          fresh.push(entry.row);
          next += 1;
        }
        if (fresh.length > 0) {
          appendRows(sql, fresh, Date.now());
          this.backupDirty();
        }
        // Host output must not feed a wake loop. Legacy senders without a
        // hint keep their separate nudge path; hints are authenticated
        // routing, never permission to execute (the host still validates its
        // document). One receipt per chat: the newest hint wins.
        if (wake) queueChatWake(sql, wake.chatId, wake.hostDevice, wake.userId);
        await this.scheduleAlarm();
        known = { head: next - 1, index };
      });
    } catch (error) {
      console.error("chat2 flush failed", { rows: batch.length, error: String(error) });
      for (const entry of batch) {
        // No ack: the pusher's outbox keeps the batch. Closing makes it
        // reconnect and replay instead of waiting on this socket forever.
        for (const { ws } of entry.sockets) {
          try {
            ws.close(1011, "storage unavailable");
          } catch {
            /* already gone */
          }
        }
        for (const { resolve } of entry.waiters) resolve({ ok: false });
      }
      return;
    }
    this.known = known;
    if (wake) this.ctx.waitUntil(this.flushWake());
    const ready = this.ctx.getWebSockets().filter((socket) => {
      const socketState = socket.deserializeAttachment() as SocketState | null;
      return socketState?.ready === true;
    });
    batch.forEach((entry, i) => {
      const { seq, dup } = outcomes[i]!;
      const { device, batchId, bytes } = entry.row;
      if (!dup) {
        for (const socket of ready) {
          if (entry.sockets.some((s) => s.ws === socket)) continue;
          send(socket, FRAME.row, { seq, device, batchId }, bytes);
        }
      }
      for (const s of entry.sockets) send(s.ws, FRAME.ack, { batchId, seq, dup: dup || s.dup });
      for (const w of entry.waiters) w.resolve({ ok: true, seq, dup: dup || w.dup });
    });
  }

  private handlePresence(
    ws: WebSocket,
    state: SocketState,
    header: Record<string, unknown>,
    payload: Uint8Array
  ): void {
    if (!state.ready || state.device === "") return;
    const at = typeof header.at === "number" ? header.at : Date.now();
    this.presence.set(state.device, at);
    this.sweepPresence();
    // Broadcast-only relay of the opaque payload — no EphemeralStore, no
    // storage; a device that joins later simply waits for the next beat.
    for (const socket of this.ctx.getWebSockets()) {
      if (socket === ws) continue;
      const socketState = socket.deserializeAttachment() as SocketState | null;
      if (!socketState?.ready) continue;
      send(socket, FRAME.presence, { device: state.device, at }, payload);
    }
  }

  private sweepPresence(): void {
    const horizon = Date.now() - PRESENCE_TTL_MS;
    for (const [device, at] of this.presence) {
      if (at < horizon) this.presence.delete(device);
    }
  }

  /** Rolling per-device quota. True = admitted. */
  private admitQuota(device: string, bytes: number): boolean {
    const now = Date.now();
    const key = device === "" ? "(unknown)" : device;
    let window = this.quotas.get(key);
    if (!window || now - window.since > QUOTA_WINDOW_MS) {
      window = { since: now, pushes: 0, bytes: 0 };
      this.quotas.set(key, window);
    }
    window.pushes += 1;
    window.bytes += bytes;
    return window.pushes <= QUOTA_MAX_PUSHES && window.bytes <= QUOTA_MAX_BYTES;
  }

  private recordPush(device: string, ok: boolean): void {
    this.outcomes.record(device, ok);
  }

  private backupDirty(): void {
    const sql = this.sql;
    if (getMeta(sql, "backupDirty") !== "1") setMeta(sql, "backupDirty", "1");
    if (!getMeta(sql, "backupDue")) setMeta(sql, "backupDue", String(Date.now() + DAY_MS));
  }

  private async markBackupDirty(): Promise<void> {
    await this.ctx.storage.transaction(async () => {
      this.backupDirty();
      await this.scheduleAlarm();
    });
  }

  /** Always called inside a storage transaction: a backup can't overwrite
   * an earlier wake alarm, including when requests interleave with retries. */
  private async scheduleAlarm(): Promise<void> {
    const sql = this.sql;
    const wake = pendingChatWake(sql);
    const backup = getMeta(sql, "backupDirty") === "1"
      ? Number(getMeta(sql, "backupDue") ?? Date.now() + DAY_MS) : Infinity;
    const next = Math.min(wake?.next_at ?? Infinity, backup);
    if (!Number.isFinite(next)) return;
    const existing = await this.ctx.storage.getAlarm();
    if (existing === null || next < existing) await this.ctx.storage.setAlarm(next);
  }

  private flushWake(): Promise<void> {
    if (this.wakeFlight) return this.wakeFlight;
    this.wakeFlight = this.deliverWake().finally(() => { this.wakeFlight = undefined; });
    return this.wakeFlight;
  }

  private async deliverWake(): Promise<void> {
    const sql = this.sql;
    const wake = pendingChatWake(sql);
    if (!wake) return;
    let accepted = false;
    let rejected = false;
    try {
      const room = this.env.DEVICE_ROOMS.get(this.env.DEVICE_ROOMS.idFromName(`d2/${wake.host_device}`));
      const response = await room.fetch("https://device/nudge", {
        method: "POST", headers: { [AUTH_USER_HEADER]: wake.user_id },
        body: JSON.stringify({ chatId: wake.chat_id }), signal: AbortSignal.timeout(5000)
      });
      accepted = response.ok;
      // Another owner's device (403) or a chat id it refuses (400): no retry
      // can succeed. Unclaimed (404) and queue-full (503) retry.
      rejected = PERMANENT_WAKE_REJECTIONS.has(response.status);
      await response.arrayBuffer();
    } catch {
      // A failed forward remains owned by this room, even with no viewer.
    }
    const exhausted = !accepted && wake.attempts + 1 >= MAX_WAKE_ATTEMPTS;
    if (rejected || exhausted) {
      console.warn("chat2 host wake dropped", {
        chat: wake.chat_id, host: wake.host_device, rejected, attempts: wake.attempts + 1
      });
    }
    await this.ctx.storage.transaction(async () => {
      if (accepted || rejected || exhausted) finishChatWake(sql, wake.token);
      else retryChatWake(sql, wake);
      await this.scheduleAlarm();
    });
  }

  /** Wake retry + nightly backup share the DO's single durable alarm. */
  async alarm(): Promise<void> {
    await this.flush();
    try {
      const wake = pendingChatWake(this.sql);
      if (wake && wake.next_at <= Date.now()) await this.flushWake();
      const due = Number(getMeta(this.sql, "backupDue") ?? "0");
      if (due <= Date.now()) {
        try { await this.backup(); }
        catch (error) {
          // Do not exhaust the runtime's finite automatic retries or spin
          // on an expired deadline during an R2 outage.
          setMeta(this.sql, "backupDue", String(Date.now() + 60_000));
          console.warn("chat2 backup retrying", error);
        }
      }
    } finally {
      await this.ctx.storage.transaction(async () => { await this.scheduleAlarm(); });
    }
  }

  /** Seq-monotonic backup; concurrent new rows retain their dirty flag. */
  private async backup(): Promise<void> {
    const sql = this.sql;
    if (getMeta(sql, "backupDirty") !== "1") return; // idle: stop the chain
    const head = headSeq(sql);
    if (head > Number(getMeta(sql, "backupSeq") ?? "0")) {
      const rows = [...rowsAfter(sql, 0)].map((row) => ({
        seq: row.seq,
        device: row.device,
        batchId: row.batchId,
        bytes: encodeBase64(row.bytes)
      }));
      const checkpoint = this.blobs.get(CHECKPOINT_BLOB);
      const frontier = this.blobs.get(FRONTIER_BLOB);
      await this.env.BLOBS.put(
        `backup/chat2/${this.ctx.id.toString()}/latest.json`,
        JSON.stringify({
          at: Date.now(),
          ...logStats(sql),
          checkpoint: checkpoint ? encodeBase64(checkpoint) : null,
          frontier: frontier ? encodeBase64(frontier) : null,
          rows
        })
      );
      setMeta(sql, "backupSeq", String(head));
    }
    if (headSeq(sql) === head) {
      setMeta(sql, "backupDirty", "0");
      sql.exec("DELETE FROM meta WHERE key='backupDue'");
    } else {
      // Rows landed during the upload: they stay dirty for the NEXT nightly
      // backup. Leaving the passed deadline re-armed the alarm at once and
      // re-uploaded the whole log back to back while the chat streamed.
      setMeta(sql, "backupDue", String(Date.now() + DAY_MS));
    }
  }
}

const send = (
  ws: WebSocket,
  type: (typeof FRAME)[keyof typeof FRAME],
  header: Record<string, unknown>,
  payload?: Uint8Array
): void => {
  try {
    const frame = encodeFrame(type, header, payload);
    ws.send(frame.buffer.slice(frame.byteOffset, frame.byteOffset + frame.byteLength) as ArrayBuffer);
  } catch {
    /* socket already gone; hibernation API cleans it up */
  }
};

const json = (value: unknown, status = 200): Response =>
  new Response(JSON.stringify(value), {
    status,
    headers: { "content-type": "application/json" }
  });

/** `bytes=N-` (open-ended resume) only; anything fancier is ignored → 200. */
const parseRangeStart = (header: string | null): number | null => {
  const match = header?.match(/^bytes=(\d+)-$/);
  if (!match) return null;
  const start = Number(match[1]);
  return Number.isSafeInteger(start) && start > 0 ? start : null;
};

const encodeBase64 = (bytes: Uint8Array): string => {
  let bin = "";
  for (let i = 0; i < bytes.length; i += 0x8000) {
    bin += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  }
  return btoa(bin);
};

/** Standard base64 (empty string ⇒ empty frontier). `undefined` = malformed. */
const decodeBase64 = (text: string): Uint8Array | undefined => {
  try {
    const bin = atob(text);
    const out = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
    return out;
  } catch {
    return undefined;
  }
};
