/**
 * Anonymous usage telemetry ingest: `POST /telemetry/v1/ping`.
 *
 * The desktop engine sends at most one ping per install per UTC day
 * (crates/engine/src/telemetry.rs; payload `zeron_proto::TelemetryPing`,
 * documented field by field in docs/PRIVACY.md). This endpoint is
 * unauthenticated, so it is strict:
 *
 *  - body at most 4 KB, JSON, exactly the documented fields (unknown fields
 *    are rejected, not dropped), every value range-checked;
 *  - per-IP rate limit (`TELEMETRY_RATE_LIMITER`), but the IP is never
 *    stored, and neither is the user agent or any header;
 *  - stored as one row per (install_id, day) in the `TelemetryStore`
 *    Durable Object's SQLite (telemetry-store.ts): retries upsert onto it.
 *
 * Also counts legacy update-feed hits per day (`countUpdateCheck`). Rows past
 * the 400-day retention are pruned by the store itself, once a day, on the
 * first write of the day.
 */
import type { Env } from "./env";
import type { TelemetryStore } from "./telemetry-store";

export const MAX_PING_BYTES = 4096;
export const RETENTION_DAYS = 400;

/** Mirrors `zeron_proto::TELEMETRY_HARNESSES` plus `other`. */
export const HARNESS_IDS: ReadonlySet<string> = new Set([
  "claude-code",
  "codex",
  "cursor",
  "devin",
  "grok",
  "hermes",
  "pi",
  "muse",
  "opencode",
  "antigravity",
  "mock",
  "unknown",
  "other"
]);

const PING_FIELDS = ["v", "install_id", "day", "app_version", "os", "arch", "first_seen", "active", "usage"];
const PING_REQUIRED = PING_FIELDS.filter((f) => f !== "usage");
const USAGE_FIELDS = [
  "day",
  "chats_started",
  "turns_finished",
  "approvals_answered",
  "zoomie_runs",
  "harnesses",
  "tokens",
  "spend_usd"
];
const TOKEN_FIELDS = ["input", "output", "cache_read", "cache_write"];

const UUID_V4 = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;
const VERSION = /^\d{1,3}\.\d{1,3}\.\d{1,4}(-[0-9A-Za-z.]{1,24})?$/;
const DATE = /^\d{4}-\d{2}-\d{2}$/;
const OSES: ReadonlySet<string> = new Set(["macos", "linux", "windows", "other"]);
const ARCHES: ReadonlySet<string> = new Set(["aarch64", "x86_64", "other"]);
/** Nothing predates Godspeed. */
const EPOCH = "2026-01-01";

const MAX_COUNT = 1_000_000;
const MAX_TOKENS = 1e13;
const MAX_SPEND = 1_000_000;

export interface PingRow {
  install_id: string;
  day: string;
  v: number;
  app_version: string;
  os: string;
  arch: string;
  first_seen: string;
  active: number;
  usage_day: string | null;
  chats_started: number;
  turns_finished: number;
  approvals_answered: number;
  zoomie_runs: number;
  harnesses: string;
  input_tokens: number;
  output_tokens: number;
  cache_read_tokens: number;
  cache_write_tokens: number;
  spend_usd: number;
}

export type Validated = { ok: true; row: PingRow } | { ok: false; error: string };

/** `YYYY-MM-DD` of a Date in UTC. */
export const utcDay = (at: Date): string => at.toISOString().slice(0, 10);

export const addDays = (day: string, days: number): string => {
  const at = new Date(`${day}T00:00:00Z`);
  at.setUTCDate(at.getUTCDate() + days);
  return utcDay(at);
};

const isObject = (value: unknown): value is Record<string, unknown> =>
  typeof value === "object" && value !== null && !Array.isArray(value);

const isDate = (value: unknown): value is string => {
  if (typeof value !== "string" || !DATE.test(value)) return false;
  const at = new Date(`${value}T00:00:00Z`);
  return !Number.isNaN(at.getTime()) && utcDay(at) === value;
};

const isCount = (value: unknown, max = MAX_COUNT): value is number =>
  typeof value === "number" && Number.isInteger(value) && value >= 0 && value <= max;

const exactKeys = (value: Record<string, unknown>, allowed: string[], required: string[]): string | undefined => {
  for (const key of Object.keys(value)) if (!allowed.includes(key)) return `unknown field: ${key.slice(0, 40)}`;
  for (const key of required) if (!(key in value)) return `missing field: ${key}`;
  return undefined;
};

/** Validate a parsed ping against the documented schema. `now` bounds the
 * dates (a day may be one off either way for clock skew). */
export const validatePing = (body: unknown, now: Date): Validated => {
  const fail = (error: string): Validated => ({ ok: false, error });
  if (!isObject(body)) return fail("expected an object");
  const keys = exactKeys(body, PING_FIELDS, PING_REQUIRED);
  if (keys) return fail(keys);
  if (body.v !== 1) return fail("unsupported version");
  if (typeof body.install_id !== "string" || !UUID_V4.test(body.install_id)) return fail("bad install_id");
  const today = utcDay(now);
  if (!isDate(body.day) || body.day < addDays(today, -1) || body.day > addDays(today, 1)) return fail("bad day");
  if (typeof body.app_version !== "string" || !VERSION.test(body.app_version)) return fail("bad app_version");
  if (typeof body.os !== "string" || !OSES.has(body.os)) return fail("bad os");
  if (typeof body.arch !== "string" || !ARCHES.has(body.arch)) return fail("bad arch");
  if (!isDate(body.first_seen) || body.first_seen < EPOCH || body.first_seen > body.day) return fail("bad first_seen");
  if (typeof body.active !== "boolean") return fail("bad active");

  const row: PingRow = {
    install_id: body.install_id,
    day: body.day,
    v: 1,
    app_version: body.app_version,
    os: body.os,
    arch: body.arch,
    first_seen: body.first_seen,
    active: body.active ? 1 : 0,
    usage_day: null,
    chats_started: 0,
    turns_finished: 0,
    approvals_answered: 0,
    zoomie_runs: 0,
    harnesses: "{}",
    input_tokens: 0,
    output_tokens: 0,
    cache_read_tokens: 0,
    cache_write_tokens: 0,
    spend_usd: 0
  };
  if (body.usage === undefined) return { ok: true, row };

  const usage = body.usage;
  if (!isObject(usage)) return fail("bad usage");
  const usageKeys = exactKeys(usage, USAGE_FIELDS, USAGE_FIELDS);
  if (usageKeys) return fail(`usage: ${usageKeys}`);
  if (!isDate(usage.day) || usage.day >= body.day || usage.day < addDays(body.day, -RETENTION_DAYS)) {
    return fail("bad usage.day");
  }
  for (const key of ["chats_started", "turns_finished", "approvals_answered", "zoomie_runs"] as const) {
    if (!isCount(usage[key])) return fail(`bad usage.${key}`);
  }
  if (!isObject(usage.harnesses)) return fail("bad usage.harnesses");
  const harnesses: Record<string, number> = {};
  const entries = Object.entries(usage.harnesses);
  if (entries.length > HARNESS_IDS.size) return fail("bad usage.harnesses");
  for (const [id, count] of entries) {
    if (!HARNESS_IDS.has(id)) return fail("unknown harness id");
    if (!isCount(count)) return fail("bad harness count");
    harnesses[id] = count;
  }
  const tokens = usage.tokens;
  if (!isObject(tokens)) return fail("bad usage.tokens");
  const tokenKeys = exactKeys(tokens, TOKEN_FIELDS, TOKEN_FIELDS);
  if (tokenKeys) return fail(`usage.tokens: ${tokenKeys}`);
  for (const key of TOKEN_FIELDS) if (!isCount(tokens[key], MAX_TOKENS)) return fail(`bad usage.tokens.${key}`);
  const spend = usage.spend_usd;
  if (typeof spend !== "number" || !Number.isFinite(spend) || spend < 0 || spend > MAX_SPEND) {
    return fail("bad usage.spend_usd");
  }

  return {
    ok: true,
    row: {
      ...row,
      usage_day: usage.day,
      chats_started: usage.chats_started as number,
      turns_finished: usage.turns_finished as number,
      approvals_answered: usage.approvals_answered as number,
      zoomie_runs: usage.zoomie_runs as number,
      harnesses: JSON.stringify(harnesses),
      input_tokens: tokens.input as number,
      output_tokens: tokens.output as number,
      cache_read_tokens: tokens.cache_read as number,
      cache_write_tokens: tokens.cache_write as number,
      spend_usd: Math.round(spend * 10_000) / 10_000
    }
  };
};

const json = (value: unknown, status = 200, headers: Record<string, string> = {}): Response =>
  new Response(JSON.stringify(value), {
    status,
    headers: { "content-type": "application/json", ...headers }
  });

/** The single telemetry store (one SQLite Durable Object holds every row). */
export const telemetryStore = (env: Env): DurableObjectStub<TelemetryStore> | undefined =>
  env.TELEMETRY ? env.TELEMETRY.get(env.TELEMETRY.idFromName(env.TELEMETRY_STORE ?? "telemetry-v1")) : undefined;

/** `POST /telemetry/v1/ping`. 204 stored, 400 invalid, 413 too large, 429
 * rate limited, 503 when no database is bound. */
export const handleTelemetryPing = async (request: Request, env: Env, now = new Date()): Promise<Response> => {
  if (request.method !== "POST") return json({ error: "method not allowed" }, 405, { allow: "POST" });
  const store = telemetryStore(env);
  if (!store) return json({ error: "telemetry not configured" }, 503);
  if (env.TELEMETRY_RATE_LIMITER) {
    // The IP keys the limiter only; it is never logged or stored.
    const ip = request.headers.get("cf-connecting-ip") ?? "unknown";
    try {
      const { success } = await env.TELEMETRY_RATE_LIMITER.limit({ key: `telemetry:${ip}` });
      if (!success) return json({ error: "rate_limited" }, 429, { "retry-after": "60" });
    } catch {
      /* a limiter failure never blocks a ping */
    }
  }
  const declared = Number(request.headers.get("content-length") ?? "0");
  if (declared > MAX_PING_BYTES) return json({ error: "too_large" }, 413);
  const text = await readCapped(request, MAX_PING_BYTES);
  if (text === undefined) return json({ error: "too_large" }, 413);
  let body: unknown;
  try {
    body = JSON.parse(text);
  } catch {
    return json({ error: "invalid json" }, 400);
  }
  const validated = validatePing(body, now);
  if (!validated.ok) return json({ error: validated.error }, 400);
  await store.storePing(validated.row);
  return new Response(null, { status: 204 });
};

/** The body as text, or `undefined` past `max` bytes (chunked bodies carry
 * no content-length, so the read itself is capped). */
const readCapped = async (request: Request, max: number): Promise<string | undefined> => {
  if (!request.body) return "";
  const reader = request.body.getReader();
  const chunks: Uint8Array[] = [];
  let size = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    size += value.byteLength;
    if (size > max) {
      await reader.cancel();
      return undefined;
    }
    chunks.push(value);
  }
  const all = new Uint8Array(size);
  let offset = 0;
  for (const chunk of chunks) {
    all.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return new TextDecoder().decode(all);
};

/** One more legacy update-feed hit today (apps up to 0.5.6). */
export const countUpdateCheck = async (env: Env, now = new Date()): Promise<void> => {
  await telemetryStore(env)?.countUpdateCheck(utcDay(now));
};
