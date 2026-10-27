/**
 * chat2 log storage (docs/chat2-sync.md workstream B) — the dumb-relay data
 * model, pure over a DO's SqlStorage + BlobStore so the workerd test tier
 * exercises it against real SQLite (the ~2MB value-cap runtime).
 *
 * The room is an append-only log of opaque Loro update blobs (packed several
 * to a SQLite row, see `chunks`) plus one client-written checkpoint blob. No wasm, no replay, no materialization:
 * cold start is a table read, so the s2 wedge class (CPU-limited history
 * replay in the DO) cannot exist here by construction.
 */

import type { BlobStore } from "./blobs";

/** Per-row byte cap, rejected at the frame header. Post-strip updates are
 * KB-scale; a full checkpoint travels over HTTP, never as a row. Well under
 * the ~2MB SQL value cap, so a row is never split across SQL values (unlike
 * s2's update-log, whose silent SQLITE_TOOBIG overflow was the 2026-08-05
 * whale freeze); chunks pack whole rows up to the same cap. */
export const MAX_ROW_BYTES = 1024 * 1024;

/** Blob-store names for the checkpoint payload and its frontier. */
export const CHECKPOINT_BLOB = "checkpoint";
export const FRONTIER_BLOB = "checkpoint-frontier";

export interface LogRow {
  seq: number;
  device: string;
  batchId: string;
  bytes: Uint8Array;
}

/** One update waiting to be appended (its seq is assigned on append). */
export interface NewRow {
  device: string;
  batchId: string;
  bytes: Uint8Array;
}

/** A chunk packs consecutive log rows into ONE SQLite row: rows written is
 * the free plan's scarce budget (100k/day, account-wide), and a streaming
 * turn pushes several updates a second. Row `i` of a chunk has seq
 * `first_seq + i`; its device, batch id and payload length sit in `heads`,
 * its bytes at the matching offset of `bytes`. */
export const CHUNK_MAX_BYTES = MAX_ROW_BYTES;

type ChunkHead = [device: string, batchId: string, length: number];

export const ensureChatLog = (sql: SqlStorage): void => {
  // Legacy one-row-per-update log. New rows go to `chunks`; these drain as
  // checkpoints cover them. Every legacy seq is below every chunk seq.
  sql.exec(
    "CREATE TABLE IF NOT EXISTS rows (seq INTEGER PRIMARY KEY, device TEXT NOT NULL, batch_id TEXT NOT NULL UNIQUE, bytes BLOB NOT NULL, received_at INTEGER NOT NULL)"
  );
  sql.exec(
    "CREATE TABLE IF NOT EXISTS chunks (first_seq INTEGER PRIMARY KEY, last_seq INTEGER NOT NULL, size INTEGER NOT NULL, heads TEXT NOT NULL, bytes BLOB NOT NULL, received_at INTEGER NOT NULL)"
  );
  sql.exec("CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)");
};

export const getMeta = (sql: SqlStorage, key: string): string | undefined => {
  const rows = [...sql.exec("SELECT value FROM meta WHERE key = ?", key)];
  return rows[0]?.value as string | undefined;
};

export const setMeta = (sql: SqlStorage, key: string, value: string): void => {
  sql.exec(
    "INSERT INTO meta (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    key,
    value
  );
};

/** The newest seq ever assigned. Derived from the tables, so appending costs
 * no counter write; `meta.headSeq` only remembers it once a checkpoint has
 * deleted the rows that carried it (and holds pre-chunk rooms' value). */
export const headSeq = (sql: SqlStorage): number => {
  const meta = Number(getMeta(sql, "headSeq") ?? "0");
  const chunk = [...sql.exec("SELECT last_seq FROM chunks ORDER BY first_seq DESC LIMIT 1")][0];
  const legacy = [...sql.exec("SELECT MAX(seq) AS seq FROM rows")][0];
  return Math.max(meta, Number(chunk?.last_seq ?? 0), Number(legacy?.seq ?? 0));
};

/** Rows with `seq <= seqFloor` are gone — covered by the checkpoint. A cursor
 * below the floor must load the checkpoint before requesting rows. */
export const seqFloor = (sql: SqlStorage): number => Number(getMeta(sql, "seqFloor") ?? "0");

export type AppendOutcome =
  | { ok: true; seq: number; dup: boolean }
  | { ok: false; error: "too_large" | "empty" };

/** Size gate for one pushed update. */
export const rowError = (bytes: Uint8Array): "too_large" | "empty" | undefined =>
  bytes.byteLength === 0 ? "empty" : bytes.byteLength > MAX_ROW_BYTES ? "too_large" : undefined;

/** batch id → seq for every stored row (the reconnect-replay dedupe set).
 * One scan of the chunk headers; callers cache it per head. */
export const batchIndex = (sql: SqlStorage): Map<string, number> => {
  const index = new Map<string, number>();
  for (const raw of sql.exec("SELECT seq, batch_id FROM rows")) {
    index.set(raw.batch_id as string, raw.seq as number);
  }
  for (const raw of sql.exec("SELECT first_seq, heads FROM chunks")) {
    const heads = JSON.parse(raw.heads as string) as ChunkHead[];
    heads.forEach(([, batchId], i) => index.set(batchId, (raw.first_seq as number) + i));
  }
  return index;
};

/** Append validated, de-duplicated rows as few chunk rows as the size cap
 * allows (one, for a coalesced streaming window). Returns the first seq. */
export const appendRows = (sql: SqlStorage, rows: NewRow[], receivedAt: number): number => {
  const first = headSeq(sql) + 1;
  let seq = first;
  let start = 0;
  while (start < rows.length) {
    let end = start;
    let size = 0;
    while (end < rows.length && (end === start || size + rows[end]!.bytes.byteLength <= CHUNK_MAX_BYTES)) {
      size += rows[end]!.bytes.byteLength;
      end += 1;
    }
    const part = rows.slice(start, end);
    const bytes = new Uint8Array(size);
    let offset = 0;
    for (const row of part) {
      bytes.set(row.bytes, offset);
      offset += row.bytes.byteLength;
    }
    const heads: ChunkHead[] = part.map((row) => [row.device, row.batchId, row.bytes.byteLength]);
    sql.exec(
      "INSERT INTO chunks (first_seq, last_seq, size, heads, bytes, received_at) VALUES (?, ?, ?, ?, ?, ?)",
      seq,
      seq + part.length - 1,
      size,
      JSON.stringify(heads),
      bytes.buffer,
      receivedAt
    );
    seq += part.length;
    start = end;
  }
  return first;
};

/** Append one update row. Batch ids dedupe reconnect re-pushes server-side:
 * a replay acks the ORIGINAL seq and appends nothing (Loro re-import is a
 * no-op client-side, so duplicates are safe end-to-end — this just keeps the
 * log tight). The room coalesces pushes through `appendRows`; this is the
 * single-row form. */
export const appendRow = (
  sql: SqlStorage,
  device: string,
  batchId: string,
  bytes: Uint8Array,
  receivedAt: number
): AppendOutcome => {
  const error = rowError(bytes);
  if (error) return { ok: false, error };
  const existing = batchIndex(sql).get(batchId);
  if (existing !== undefined) return { ok: true, seq: existing, dup: true };
  return { ok: true, seq: appendRows(sql, [{ device, batchId, bytes }], receivedAt), dup: false };
};

const unpack = (firstSeq: number, headsJson: string, packed: ArrayBuffer): LogRow[] => {
  const heads = JSON.parse(headsJson) as ChunkHead[];
  const all = new Uint8Array(packed);
  let offset = 0;
  return heads.map(([device, batchId, length], i) => {
    const bytes = all.slice(offset, offset + length);
    offset += length;
    return { seq: firstSeq + i, device, batchId, bytes };
  });
};

/** Rows `seq > after` (and above the checkpoint floor), in order, optionally
 * excluding one device's own writes (the reconnect-after-offline-work path
 * never re-downloads itself). */
export function* rowsAfter(
  sql: SqlStorage,
  after: number,
  excludeDevice?: string
): Generator<LogRow> {
  const from = Math.max(after, seqFloor(sql));
  const legacy = excludeDevice
    ? sql.exec(
        "SELECT seq, device, batch_id, bytes FROM rows WHERE seq > ? AND device != ? ORDER BY seq",
        from,
        excludeDevice
      )
    : sql.exec("SELECT seq, device, batch_id, bytes FROM rows WHERE seq > ? ORDER BY seq", from);
  for (const raw of legacy) {
    yield {
      seq: raw.seq as number,
      device: raw.device as string,
      batchId: raw.batch_id as string,
      bytes: new Uint8Array(raw.bytes as ArrayBuffer)
    };
  }
  // The chunk holding `from + 1` may start at or below `from`: find it by
  // primary key, then range-scan everything after it.
  const straddle = [
    ...sql.exec(
      "SELECT first_seq, last_seq, heads, bytes FROM chunks WHERE first_seq <= ? ORDER BY first_seq DESC LIMIT 1",
      from
    )
  ][0];
  const chunks: Iterable<Record<string, SqlStorageValue>> = sql.exec(
    "SELECT first_seq, last_seq, heads, bytes FROM chunks WHERE first_seq > ? ORDER BY first_seq",
    from
  );
  const ordered = straddle && (straddle.last_seq as number) > from ? [straddle] : [];
  for (const raw of [ordered, chunks]) {
    for (const chunk of raw) {
      for (const row of unpack(chunk.first_seq as number, chunk.heads as string, chunk.bytes as ArrayBuffer)) {
        if (row.seq <= from) continue;
        if (excludeDevice !== undefined && row.device === excludeDevice) continue;
        yield row;
      }
    }
  }
}

/** Count and payload bytes of the rows above `floor`. */
const liveRows = (sql: SqlStorage, floor: number): { n: number; bytes: number } => {
  const legacy = [
    ...sql.exec("SELECT COUNT(*) AS n, COALESCE(SUM(LENGTH(bytes)), 0) AS b FROM rows WHERE seq > ?", floor)
  ][0]!;
  const whole = [
    ...sql.exec(
      "SELECT COALESCE(SUM(last_seq - first_seq + 1), 0) AS n, COALESCE(SUM(size), 0) AS b FROM chunks WHERE first_seq > ?",
      floor
    )
  ][0]!;
  let n = (legacy.n as number) + (whole.n as number);
  let bytes = (legacy.b as number) + (whole.b as number);
  const straddle = [
    ...sql.exec(
      "SELECT first_seq, last_seq, heads FROM chunks WHERE first_seq <= ? ORDER BY first_seq DESC LIMIT 1",
      floor
    )
  ][0];
  if (straddle && (straddle.last_seq as number) > floor) {
    const heads = JSON.parse(straddle.heads as string) as ChunkHead[];
    heads.forEach(([, , length], i) => {
      if ((straddle.first_seq as number) + i > floor) {
        n += 1;
        bytes += length;
      }
    });
  }
  return { n, bytes };
};

export type CheckpointOutcome =
  | { ok: true; seqFloor: number; pruned: number }
  | { ok: false; error: "floor_regression" | "ahead_of_head" | "empty" };

/** Commit a client-built checkpoint covering rows `seq <= seqCovered`.
 *
 * The guard is floor-monotonic — the dumb replacement for s2's VV-monotonic
 * R2 guard: a checkpoint may only move the floor forward, and may not claim
 * to cover rows that don't exist yet. Within those bounds the SERVER trusts
 * the payload blindly (owner-only rooms contain semantic garbage per-user;
 * the next checkpoint erases it). Rows covered by the checkpoint are deleted
 * in the same event — DO events are single-threaded and commit atomically,
 * so a crash never leaves the floor and the rows disagreeing. */
export const commitCheckpoint = (
  sql: SqlStorage,
  blobs: BlobStore,
  seqCovered: number,
  frontier: Uint8Array,
  bytes: Uint8Array,
  committedAt: number
): CheckpointOutcome => {
  if (bytes.byteLength === 0) return { ok: false, error: "empty" };
  if (seqCovered < seqFloor(sql)) return { ok: false, error: "floor_regression" };
  const head = headSeq(sql);
  if (seqCovered > head) return { ok: false, error: "ahead_of_head" };
  blobs.put(CHECKPOINT_BLOB, bytes);
  blobs.put(FRONTIER_BLOB, frontier);
  const before = liveRows(sql, seqFloor(sql)).n;
  sql.exec("DELETE FROM rows WHERE seq <= ?", seqCovered);
  // A chunk straddling the floor stays; readers skip its covered rows.
  sql.exec("DELETE FROM chunks WHERE last_seq <= ?", seqCovered);
  const pruned = before - liveRows(sql, seqCovered).n;
  // The rows that carried the head may be gone now.
  if (Number(getMeta(sql, "headSeq") ?? "0") !== head) setMeta(sql, "headSeq", String(head));
  setMeta(sql, "seqFloor", String(seqCovered));
  setMeta(sql, "checkpointSeq", String(seqCovered));
  setMeta(sql, "checkpointSize", String(bytes.byteLength));
  setMeta(sql, "checkpointAt", String(committedAt));
  return { ok: true, seqFloor: seqCovered, pruned };
};

export interface LogStats {
  headSeq: number;
  seqFloor: number;
  rowCount: number;
  rowBytes: number;
  checkpointSeq: number;
  checkpointSize: number;
  checkpointAt: number;
}

/** The hello/stats surface — also what the host's checkpoint policy reads
 * (`rowBytes > 512KB || rowCount > 200` → post a checkpoint) and what the
 * fleet alert watches (unbounded growth is this design's failure mode). */
export const logStats = (sql: SqlStorage): LogStats => {
  const floor = seqFloor(sql);
  const live = liveRows(sql, floor);
  return {
    headSeq: headSeq(sql),
    seqFloor: floor,
    rowCount: live.n,
    rowBytes: live.bytes,
    checkpointSeq: Number(getMeta(sql, "checkpointSeq") ?? "0"),
    checkpointSize: Number(getMeta(sql, "checkpointSize") ?? "0"),
    checkpointAt: Number(getMeta(sql, "checkpointAt") ?? "0")
  };
};
