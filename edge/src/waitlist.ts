/**
 * The iPhone beta waitlist (`POST /waitlist`) and the Worker's view of the
 * approved-email allow-list (access-store.ts holds the tables).
 *
 * `POST /waitlist` is public (the landing page's "Join the iPhone beta"
 * form), so it is strict:
 *  - JSON `{ "email": "…", "note"?: "…" }` (fetch from the landing page,
 *    CORS for its origins) or a plain HTML form post (answered with a 303
 *    back to the page);
 *  - body at most 2 KB, email validated and lowercased, note at most 500
 *    characters with control characters removed, a honeypot field
 *    (`website`) that bots fill in;
 *  - per-IP rate limit (`WAITLIST_RATE_LIMITER`); the IP is never stored;
 *  - the answer never says whether an email was already on the list.
 *
 * Approving someone on /admin/access adds their email to `allowed_emails`;
 * `allowedEmailSet` gives the bearer gate (allowlist.ts) that list, cached
 * per isolate for a minute so ordinary sync traffic never touches the store.
 */
import { ACCESS_STORE_NAME } from "./access-store";
import type { Env } from "./env";
import type { TelemetryStore } from "./telemetry-store";

export const MAX_WAITLIST_BYTES = 2048;
export const MAX_NOTE_CHARS = 500;
export const LANDING_URL = "https://godspeed-code.pages.dev";

/** Origins allowed to post from a browser: the landing page (and its Pages
 * preview deployments), the portfolio site, and local previews. */
export const corsOrigin = (origin: string | null): string | undefined => {
  if (!origin) return undefined;
  let url: URL;
  try {
    url = new URL(origin);
  } catch {
    return undefined;
  }
  const host = url.hostname;
  if (url.protocol === "https:" && (host === "godspeed-code.pages.dev" || host.endsWith(".godspeed-code.pages.dev"))) {
    return url.origin;
  }
  if (url.protocol === "https:" && (host === "shrithan.site" || host === "www.shrithan.site")) return url.origin;
  if (url.protocol === "http:" && (host === "localhost" || host === "127.0.0.1")) return url.origin;
  return undefined;
};

const EMAIL =
  /^[a-z0-9.!#$%&'*+/=?^_`{|}~-]{1,64}@(?:[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?\.)+[a-z]{2,63}$/;

/** A lowercased, trimmed email, or `undefined` if it isn't one. */
export const normalizeEmail = (value: unknown): string | undefined => {
  if (typeof value !== "string") return undefined;
  const email = value.trim().toLowerCase();
  if (email.length > 254 || !EMAIL.test(email)) return undefined;
  if (email.startsWith(".") || email.includes("..") || email.split("@")[0].endsWith(".")) return undefined;
  return email;
};

export type WaitlistInput =
  | { ok: true; email: string; note: string; bot: boolean }
  | { ok: false; error: string };

const isObject = (value: unknown): value is Record<string, unknown> =>
  typeof value === "object" && value !== null && !Array.isArray(value);

/** Validate a parsed body. Unknown fields are rejected, except the honeypot. */
export const validateWaitlist = (body: unknown): WaitlistInput => {
  if (!isObject(body)) return { ok: false, error: "expected an object" };
  for (const key of Object.keys(body)) {
    if (!["email", "note", "website"].includes(key)) return { ok: false, error: `unknown field: ${key.slice(0, 40)}` };
  }
  const email = normalizeEmail(body.email);
  if (!email) return { ok: false, error: "Enter a valid email address." };
  if (body.note !== undefined && typeof body.note !== "string") return { ok: false, error: "bad note" };
  const note = (typeof body.note === "string" ? body.note : "")
    .replace(/[\u0000-\u0008\u000B-\u001F\u007F-\u009F\u2028\u2029]/g, " ")
    .trim();
  if ([...note].length > MAX_NOTE_CHARS) return { ok: false, error: `Keep the note under ${MAX_NOTE_CHARS} characters.` };
  const bot = typeof body.website === "string" && body.website.trim() !== "";
  return { ok: true, email, note, bot };
};

/** The access instance of the store, or `undefined` when unbound. */
export const accessStore = (env: Env): DurableObjectStub<TelemetryStore> | undefined =>
  env.TELEMETRY ? env.TELEMETRY.get(env.TELEMETRY.idFromName(env.ACCESS_STORE ?? ACCESS_STORE_NAME)) : undefined;

// ---------------------------------------------------------------------------
// Approved emails, cached per isolate
// ---------------------------------------------------------------------------

const ALLOWED_TTL_MS = 60 * 1000;
let allowedCache: { emails: ReadonlySet<string>; at: number; store: string } | undefined;

export type AllowedSet =
  | { kind: "none" }
  | { kind: "list"; emails: ReadonlySet<string> }
  | { kind: "error" };

/** The approved emails. `none` = no store bound (self-hosted edges without
 * one keep the secret-only behaviour); `error` = the store couldn't be read
 * and nothing was cached yet. A stale list is used while the store is down. */
export const allowedEmailSet = async (env: Env, nowMs = Date.now()): Promise<AllowedSet> => {
  const store = accessStore(env);
  if (!store) return { kind: "none" };
  const name = env.ACCESS_STORE ?? ACCESS_STORE_NAME;
  const cached = allowedCache?.store === name ? allowedCache : undefined;
  if (cached && nowMs - cached.at < ALLOWED_TTL_MS) return { kind: "list", emails: cached.emails };
  try {
    const emails = new Set(await store.allowedEmails());
    allowedCache = { emails, at: nowMs, store: name };
    return { kind: "list", emails };
  } catch (e) {
    console.warn("access store unavailable", e instanceof Error ? e.message : String(e));
    return cached ? { kind: "list", emails: cached.emails } : { kind: "error" };
  }
};

/** Drop the cached list (after an approve/revoke here, and in tests). */
export const clearAllowedCache = (): void => {
  allowedCache = undefined;
};

// ---------------------------------------------------------------------------
// POST /waitlist
// ---------------------------------------------------------------------------

const json = (value: unknown, status: number, headers: Record<string, string> = {}): Response =>
  new Response(JSON.stringify(value), {
    status,
    headers: { "content-type": "application/json", "cache-control": "no-store", ...headers }
  });

const corsHeaders = (origin: string | undefined): Record<string, string> =>
  origin
    ? {
        "access-control-allow-origin": origin,
        "access-control-allow-methods": "POST, OPTIONS",
        "access-control-allow-headers": "content-type",
        "access-control-max-age": "86400",
        vary: "origin"
      }
    : { vary: "origin" };

/** Where a plain form post goes back to (the page's own beta section). */
const formRedirect = (request: Request, result: "joined" | "invalid" | "busy"): Response => {
  const origin = corsOrigin(request.headers.get("origin")) ?? LANDING_URL;
  return new Response(null, {
    status: 303,
    headers: { location: `${origin}/?beta=${result}#iphone`, "cache-control": "no-store" }
  });
};

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

/** `POST /waitlist` (and its CORS preflight). 200 `{ok:true}` stored (or
 * already there), 400 invalid, 413 too large, 429 rate limited, 503 store
 * unavailable or full. */
export const handleWaitlist = async (request: Request, env: Env, nowMs = Date.now()): Promise<Response> => {
  const origin = corsOrigin(request.headers.get("origin"));
  const cors = corsHeaders(origin);
  if (request.method === "OPTIONS") return new Response(null, { status: 204, headers: cors });
  if (request.method !== "POST") return json({ error: "method not allowed" }, 405, { allow: "POST, OPTIONS", ...cors });

  const contentType = (request.headers.get("content-type") ?? "").toLowerCase();
  const form = contentType.startsWith("application/x-www-form-urlencoded");
  if (!form && !contentType.startsWith("application/json")) return json({ error: "unsupported content type" }, 415, cors);

  const store = accessStore(env);
  if (!store) return form ? formRedirect(request, "busy") : json({ error: "waitlist not configured" }, 503, cors);

  if (env.WAITLIST_RATE_LIMITER) {
    // The IP keys the limiter only; it is never logged or stored.
    const ip = request.headers.get("cf-connecting-ip") ?? "unknown";
    try {
      const { success } = await env.WAITLIST_RATE_LIMITER.limit({ key: `waitlist:${ip}` });
      if (!success) {
        return form
          ? formRedirect(request, "busy")
          : json({ error: "rate_limited", message: "Too many tries. Wait a minute and try again." }, 429, { "retry-after": "60", ...cors });
      }
    } catch {
      /* a limiter failure never blocks a sign-up */
    }
  }

  const declared = Number(request.headers.get("content-length") ?? "0");
  if (declared > MAX_WAITLIST_BYTES) return json({ error: "too_large" }, 413, cors);
  const text = await readCapped(request, MAX_WAITLIST_BYTES);
  if (text === undefined) return json({ error: "too_large" }, 413, cors);

  let body: unknown;
  if (form) {
    body = Object.fromEntries(new URLSearchParams(text));
  } else {
    try {
      body = JSON.parse(text);
    } catch {
      return json({ error: "invalid json" }, 400, cors);
    }
  }
  const input = validateWaitlist(body);
  if (!input.ok) return form ? formRedirect(request, "invalid") : json({ error: "invalid", message: input.error }, 400, cors);
  // Bots that fill the honeypot get the same answer as everyone else.
  if (!input.bot) {
    try {
      const result = await store.waitlistJoin(input.email, input.note, nowMs);
      if (result === "full") {
        return form ? formRedirect(request, "busy") : json({ error: "full", message: "The waitlist is full right now." }, 503, cors);
      }
    } catch (e) {
      console.warn("waitlist store unavailable", e instanceof Error ? e.message : String(e));
      return form
        ? formRedirect(request, "busy")
        : json({ error: "unavailable", message: "Couldn't save that right now. Try again later." }, 503, { "retry-after": "300", ...cors });
    }
  }
  return form ? formRedirect(request, "joined") : json({ ok: true }, 200, cors);
};
