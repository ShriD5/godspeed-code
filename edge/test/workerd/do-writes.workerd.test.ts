import { env, runInDurableObject } from "cloudflare:test";
import { describe, expect, it } from "vitest";
import { decodeFrame, encodeFrame, FRAME } from "../../src/chat-frames";
import { logStats } from "../../src/chat-log";
import type { ChatRoom } from "../../src/chat-room";
import { AUTH_USER_HEADER } from "../../src/env";
import type { Op, Row } from "../../src/registry-core";
import type { RegistryRoom } from "../../src/registry-room";

/** The free plan's 100k rows written per day is account-wide. These pin the
 * write cost of the two hot rooms: a streaming chat turn and a running
 * session's heartbeats — and that the savings lose no update. */

const H = { [AUTH_USER_HEADER]: "owner" };
const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

// ── chat2 ──────────────────────────────────────────────────────────────────

const chatRoom = (name: string) => env.CHAT_ROOMS.get(env.CHAT_ROOMS.idFromName(`chat2/${name}`));

interface Received {
  type: number;
  header: Record<string, unknown>;
  payload: Uint8Array;
}

async function chatSocket(stub: DurableObjectStub, device: string) {
  const response = await stub.fetch(`https://chat/ws?device=${device}&chatId=c`, {
    headers: { ...H, upgrade: "websocket" }
  });
  expect(response.status).toBe(101);
  const ws = response.webSocket!;
  ws.accept();
  ws.binaryType = "arraybuffer";
  const frames: Received[] = [];
  ws.addEventListener("message", (event) => {
    const frame = decodeFrame(new Uint8Array(event.data as ArrayBuffer))!;
    frames.push({ type: frame.type, header: frame.header, payload: frame.payload });
  });
  ws.send(encodeFrame(FRAME.hello, { device, cursor: 0 }));
  await expect.poll(() => frames.some((f) => f.type === FRAME.state)).toBe(true);
  const of = (type: number) => frames.filter((f) => f.type === type);
  return { ws, frames, acks: () => of(FRAME.ack), rows: () => of(FRAME.row) };
}

const update = (i: number) => new TextEncoder().encode(`update ${i} ${"x".repeat(i % 40)}`);

/** The durable log, read back the way a fresh device loads it. */
async function pullRows(stub: DurableObjectStub) {
  const response = await stub.fetch("https://chat/rows?after=0&device=fresh", { headers: H });
  const body = new Uint8Array(await response.arrayBuffer());
  const rows: Array<{ seq: number; batchId: string; bytes: Uint8Array }> = [];
  for (let offset = 0; offset < body.length; ) {
    const length = new DataView(body.buffer, body.byteOffset).getUint32(offset, true);
    offset += 4;
    const frame = decodeFrame(body.subarray(offset, offset + length))!;
    offset += length;
    if (frame.type === FRAME.row) {
      rows.push({ seq: frame.header.seq as number, batchId: frame.header.batchId as string, bytes: frame.payload });
    }
  }
  return rows;
}

const chatWrites = (stub: DurableObjectStub) =>
  runInDurableObject(stub, (instance: ChatRoom) => instance.meter.total);

describe("chat2 room write budget", () => {
  it("coalesces a streaming turn into a few chunk writes and loses no update", async () => {
    const stub = chatRoom(crypto.randomUUID());
    // 500 updates over a simulated 30 s: the window is scaled 10x down
    // (100 ms) so the turn takes 3 s of wall time. Two pushers keep each
    // device under the per-device quota (300 pushes/min).
    await runInDurableObject(stub, (instance: ChatRoom) => {
      instance.flushIntervalMs = 100;
    });
    const host = await chatSocket(stub, "host");
    const phone = await chatSocket(stub, "phone");
    const viewer = await chatSocket(stub, "viewer");
    const before = await chatWrites(stub);
    const sent = new Map<string, Uint8Array>();
    for (let i = 0; i < 500; i++) {
      const pusher = i % 2 === 0 ? host : phone;
      const batchId = `b${i}`;
      sent.set(batchId, update(i));
      pusher.ws.send(encodeFrame(FRAME.push, { batchId }, update(i)));
      await sleep(6);
    }
    await expect.poll(() => host.acks().length + phone.acks().length, { timeout: 10_000 }).toBe(500);
    await expect.poll(() => viewer.rows().length).toBe(500);
    const written = (await chatWrites(stub)) - before;
    // One row per pushed update plus per-push bookkeeping used to cost
    // ~2,500 rows here (insert + batch-id index + headSeq + backupDirty +
    // pushOutcomes). Now: one chunk row per window (~30) plus a handful of
    // first-write meta rows.
    expect(written).toBeLessThanOrEqual(60);

    // Every ack is the seq the viewer saw, relays arrive dense and in order.
    expect(viewer.rows().map((r) => r.header.seq)).toEqual(Array.from({ length: 500 }, (_, i) => i + 1));
    const seqOf = new Map(viewer.rows().map((r) => [r.header.batchId as string, r.header.seq as number]));
    for (const ack of [...host.acks(), ...phone.acks()]) {
      expect(ack.header.dup).toBe(false);
      expect(ack.header.seq).toBe(seqOf.get(ack.header.batchId as string));
    }
    // A fresh device loading the log gets every update, byte-identical.
    const log = await pullRows(stub);
    expect(log.map((r) => r.seq)).toEqual(Array.from({ length: 500 }, (_, i) => i + 1));
    for (const row of log) expect(row.bytes).toEqual(sent.get(row.batchId));
    const stats = await runInDurableObject(stub, (_i, state) => logStats(state.storage.sql));
    expect(stats.rowCount).toBe(500);
    expect(stats.rowBytes).toBe([...sent.values()].reduce((n, b) => n + b.byteLength, 0));
    for (const s of [host, phone, viewer]) s.ws.close();
  });

  it("acks only durable rows: a crash before the flush loses nothing the client will not replay", async () => {
    const name = crypto.randomUUID();
    let stub = chatRoom(name);
    await runInDurableObject(stub, (instance: ChatRoom) => {
      instance.flushIntervalMs = 60_000;
    });
    const host = await chatSocket(stub, "host");
    // The first push after a quiet window is written at once (no added
    // latency for a lone message); the rest wait for the window.
    host.ws.send(encodeFrame(FRAME.push, { batchId: "a0" }, update(0)));
    await expect.poll(() => host.acks().length).toBe(1);
    for (let i = 1; i < 10; i++) host.ws.send(encodeFrame(FRAME.push, { batchId: `a${i}` }, update(i)));
    await expect.poll(() => runInDurableObject(stub, (instance: ChatRoom) => instance["buffer"].length)).toBe(9);
    expect(host.acks().length).toBe(1);
    expect((await pullRows(stub)).length).toBe(1);

    // Crash: the in-memory buffer is gone, never written, never acked.
    await runInDurableObject(stub, (_instance, state) => {
      try {
        state.abort("simulated crash");
      } catch {
        /* abort throws into the caller */
      }
    }).catch(() => undefined);
    stub = chatRoom(name); // an aborted stub stays broken
    expect((await pullRows(stub)).map((r) => r.batchId)).toEqual(["a0"]);

    // Reconnect: the client's outbox replays every unacked batch (and a0,
    // as if its ack had been lost too). Dedupe keeps the log exact.
    const again = await chatSocket(stub, "host");
    for (let i = 0; i < 10; i++) again.ws.send(encodeFrame(FRAME.push, { batchId: `a${i}` }, update(i)));
    await expect.poll(() => again.acks().length, { timeout: 5000 }).toBe(10);
    const acks = new Map(again.acks().map((a) => [a.header.batchId as string, a.header]));
    expect(acks.get("a0")).toEqual({ batchId: "a0", seq: 1, dup: true });
    const log = await pullRows(stub);
    expect(log.map((r) => r.batchId)).toEqual(Array.from({ length: 10 }, (_, i) => `a${i}`));
    expect(log.map((r) => r.seq)).toEqual(Array.from({ length: 10 }, (_, i) => i + 1));
    log.forEach((row, i) => expect(row.bytes).toEqual(update(i)));
    again.ws.close();
    host.ws.close();
  });

  it("writes buffered pushes when their socket closes, and replays dedupe against them", async () => {
    const stub = chatRoom(crypto.randomUUID());
    await runInDurableObject(stub, (instance: ChatRoom) => {
      instance.flushIntervalMs = 60_000;
    });
    const host = await chatSocket(stub, "host");
    for (let i = 0; i < 6; i++) host.ws.send(encodeFrame(FRAME.push, { batchId: `c${i}` }, update(i)));
    await expect.poll(() => host.acks().length).toBe(1);
    await expect.poll(() => runInDurableObject(stub, (instance: ChatRoom) => instance["buffer"].length)).toBe(5);
    host.ws.close();
    await expect.poll(async () => (await pullRows(stub)).length).toBe(6);
    await runInDurableObject(stub, (instance: ChatRoom) => {
      instance.flushIntervalMs = 50;
    });
    const again = await chatSocket(stub, "host");
    for (let i = 1; i < 6; i++) again.ws.send(encodeFrame(FRAME.push, { batchId: `c${i}` }, update(i)));
    await expect.poll(() => again.acks().length).toBe(5);
    expect(again.acks().every((a) => a.header.dup === true)).toBe(true);
    expect((await pullRows(stub)).length).toBe(6);
    again.ws.close();
  });

  it("flushes buffered rows before a checkpoint and keeps rows a straddling chunk still owes", async () => {
    const stub = chatRoom(crypto.randomUUID());
    await runInDurableObject(stub, (instance: ChatRoom) => {
      instance.flushIntervalMs = 60_000;
    });
    const host = await chatSocket(stub, "host");
    for (let i = 0; i < 5; i++) host.ws.send(encodeFrame(FRAME.push, { batchId: `k${i}` }, update(i)));
    await expect.poll(() => runInDurableObject(stub, (instance: ChatRoom) => instance["buffer"].length)).toBe(4);
    // Rows 2..5 share one chunk; a checkpoint covering 3 flushes them first
    // and keeps the chunk for rows 4 and 5.
    const response = await stub.fetch("https://chat/checkpoint?seqCovered=3", {
      method: "POST",
      headers: { ...H, "x-chat2-frontier": "AQ==" },
      body: new Uint8Array([7, 7, 7])
    });
    expect(response.status, await response.text()).toBe(200);
    await expect.poll(() => host.acks().length).toBe(5);
    expect((await pullRows(stub)).map((r) => r.batchId)).toEqual(["k3", "k4"]);
    const stats = await runInDurableObject(stub, (_i, state) => logStats(state.storage.sql));
    expect(stats).toMatchObject({ headSeq: 5, seqFloor: 3, rowCount: 2, rowBytes: update(3).length + update(4).length });
    host.ws.close();
  });
});

// ── registry ───────────────────────────────────────────────────────────────

const registryRoom = () => env.REGISTRY_ROOMS.get(env.REGISTRY_ROOMS.idFromName(crypto.randomUUID()));
let tick = 1;
const hlc = () => `${String(tick++).padStart(13, "0")}-000000-mac`;
const T0 = 1_800_000_000_000;
const sessionOp = (status: string, updatedAt: number, turn: string | null = null): Op => ({
  kind: "sessions",
  id: "c1",
  op: "upsert",
  hlc: hlc(),
  set: { chatId: "c1", deviceId: "mac", status, lastCompletedTurn: turn, updatedAt, runningSubagents: 0 }
});

async function registryPush(stub: DurableObjectStub, ops: Op[]) {
  const response = await stub.fetch("https://registry/push?device=mac", {
    method: "POST",
    headers: H,
    body: JSON.stringify({ batch: crypto.randomUUID(), ops })
  });
  expect(response.status).toBe(200);
  return response.json<{ seq: number; applied: number }>();
}

async function registrySocket(stub: DurableObjectStub) {
  const response = await stub.fetch("https://registry/ws?device=phone", { headers: { ...H, upgrade: "websocket" } });
  expect(response.status).toBe(101);
  const ws = response.webSocket!;
  ws.accept();
  const frames: Array<Record<string, unknown>> = [];
  ws.addEventListener("message", (event) => {
    frames.push(JSON.parse(event.data as string));
  });
  ws.send(JSON.stringify({ t: "hello", device: "phone", cursor: 0 }));
  await expect.poll(() => frames.some((f) => f.t === "state")).toBe(true);
  return { ws, frames };
}

const registryWrites = (stub: DurableObjectStub) =>
  runInDurableObject(stub, (instance: RegistryRoom) => instance.meter.total);

const storedSession = (stub: DurableObjectStub) =>
  runInDurableObject(stub, (_i, state) => {
    const raw = [...state.storage.sql.exec("SELECT seq, fields FROM rows WHERE kind = 'sessions' AND id = 'c1'")][0]!;
    return { seq: raw.seq as number, fields: JSON.parse(raw.fields as string) as Record<string, unknown> };
  });

describe("registry room write budget", () => {
  it("keeps a heartbeat stream mostly in memory while peers see every beat", async () => {
    const stub = registryRoom();
    await registryPush(stub, [
      { kind: "chats", id: "c1", op: "upsert", hlc: hlc(), set: { title: "Busy chat" } },
      sessionOp("working", T0)
    ]);
    const phone = await registrySocket(stub);
    const before = await registryWrites(stub);
    // Ten minutes of a running session: one keepalive touch every 10 s.
    for (let i = 1; i <= 61; i++) await registryPush(stub, [sessionOp("working", T0 + i * 10_000)]);
    const written = (await registryWrites(stub)) - before;
    // Was 61 × 5 rows (row + its seq-index entry + seq + backupDirty +
    // pushOutcomes). Now every third beat is stored (row + seq).
    expect(written).toBeLessThanOrEqual(45);

    // The peer saw all 61 beats; seq-0 frames never move its cursor and the
    // stored ones arrive contiguous (the client's apply_rows gap rule).
    const beats = phone.frames.filter((f) => f.t === "rows");
    expect(beats.length).toBe(61);
    let cursor = (phone.frames.find((f) => f.t === "state")!.seq as number);
    for (const frame of beats) {
      const seq = frame.seq as number;
      expect(seq <= cursor + 1).toBe(true);
      cursor = Math.max(cursor, seq);
    }
    expect(beats.filter((f) => f.seq === 0).length).toBe(41);
    const last = (beats.at(-1)!.rows as Row[])[0]!;
    expect(last.fields.updatedAt).toBe(T0 + 610_000);
    const stats = await (await stub.fetch("https://registry/stats", { headers: H })).json<{ seq: number }>();
    expect(cursor).toBe(stats.seq);

    // Readers get the freshest copy while the room is up...
    const live = await (await stub.fetch("https://registry/rows", { headers: H })).json<{ rows: Row[] }>();
    expect(live.rows.find((r) => r.kind === "sessions")!.fields.updatedAt).toBe(T0 + 610_000);
    // ...and storage is never more than one persist step behind.
    expect((await storedSession(stub)).fields.updatedAt).toBe(T0 + 600_000);
    phone.ws.close();
  });

  it("stores a real change at once and survives losing the in-memory heartbeat", async () => {
    const name = crypto.randomUUID();
    let stub = env.REGISTRY_ROOMS.get(env.REGISTRY_ROOMS.idFromName(name));
    await registryPush(stub, [sessionOp("working", T0)]);
    await registryPush(stub, [sessionOp("working", T0 + 10_000)]);
    const held = await storedSession(stub);
    expect(held.fields.updatedAt).toBe(T0);

    // Eviction drops the memory copy; the stored row is the last persist.
    await runInDurableObject(stub, (_instance, state) => {
      try {
        state.abort("simulated eviction");
      } catch {
        /* abort throws into the caller */
      }
    }).catch(() => undefined);
    stub = env.REGISTRY_ROOMS.get(env.REGISTRY_ROOMS.idFromName(name));
    const reloaded = await (await stub.fetch("https://registry/rows", { headers: H })).json<{ rows: Row[] }>();
    expect(reloaded.rows[0]!.fields.updatedAt).toBe(T0);

    // The next beat applies normally on top of it, and a status flip (a
    // real change, the one notifications key on) is written immediately
    // with its own seq.
    const beat = await registryPush(stub, [sessionOp("working", T0 + 20_000)]);
    expect(beat.applied).toBe(1);
    const flip = await registryPush(stub, [sessionOp("idle", T0 + 21_000, "turn-1")]);
    expect(flip.seq).toBe(held.seq + 1);
    const stored = await storedSession(stub);
    expect(stored).toEqual({
      seq: held.seq + 1,
      fields: { chatId: "c1", deviceId: "mac", status: "idle", lastCompletedTurn: "turn-1", updatedAt: T0 + 21_000, runningSubagents: 0 }
    });
  });

  it("writes nothing for a re-pushed batch that changes nothing", async () => {
    const stub = registryRoom();
    const op = sessionOp("working", T0);
    await registryPush(stub, [op]);
    const before = await registryWrites(stub);
    for (let i = 0; i < 20; i++) expect((await registryPush(stub, [op])).applied).toBe(0);
    expect((await registryWrites(stub)) - before).toBeLessThanOrEqual(1);
  });
});
