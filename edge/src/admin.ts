/**
 * The private admin dashboard: `GET /admin` (server-rendered HTML) and
 * `GET /admin/api/stats` (the same numbers as JSON).
 *
 * Sign-in reuses the edge's WorkOS AuthKit client in a browser flow:
 *   /admin            → no session ⇒ 302 to the AuthKit authorize URL
 *   /admin/callback   → code exchange (WORKOS_API_KEY) ⇒ admin check ⇒ session
 *   /admin/logout     → clears the session
 * Only emails in the `ADMIN_EMAILS` secret (comma-separated, exact,
 * case-insensitive, verified) get in; everyone else gets 403. Unset or empty
 * ⇒ nobody does (fail closed). The session is a signed cookie (HMAC-SHA256,
 * key derived from WORKOS_API_KEY) holding the email and an expiry, and the
 * email is re-checked against the current list on every request, so
 * removing someone from ADMIN_EMAILS locks them out at once.
 *
 * `/admin/access` (admin-access.ts) is the iPhone beta waitlist and the
 * approved-email allow-list; it is the only route that takes POSTs.
 */
import { ACCESS_ACTIONS, applyAccessAction, parseAccessForm, renderAccessPage, type AccessAction } from "./admin-access";
import { computeStats, parseRange } from "./admin-stats";
import { renderDashboard } from "./admin-page";
import type { Env } from "./env";
import { accessStore, normalizeEmail } from "./waitlist";
import { exchange, WorkOsAuthFailed } from "./workos";

const SESSION_COOKIE = "gs_admin";
const STATE_COOKIE = "gs_admin_state";
const SESSION_TTL_S = 12 * 60 * 60;
const STATE_TTL_S = 10 * 60;

export const parseAdmins = (raw: string | undefined): ReadonlySet<string> =>
  new Set(
    (raw ?? "")
      .split(/[,\s]+/)
      .map((e) => e.trim().toLowerCase())
      .filter((e) => e.includes("@") && !e.startsWith("@"))
  );

export const isAdmin = (env: Env, email: string): boolean =>
  parseAdmins(env.ADMIN_EMAILS).has(email.trim().toLowerCase());

// ---------------------------------------------------------------------------
// Signed session cookie
// ---------------------------------------------------------------------------

const encoder = new TextEncoder();

const b64url = (bytes: Uint8Array): string =>
  btoa(String.fromCharCode(...bytes))
    .replaceAll("+", "-")
    .replaceAll("/", "_")
    .replace(/=+$/, "");

const fromB64url = (text: string): Uint8Array => {
  const padded = text.replaceAll("-", "+").replaceAll("_", "/") + "=".repeat((4 - (text.length % 4)) % 4);
  return Uint8Array.from(atob(padded), (c) => c.charCodeAt(0));
};

const sessionKey = async (env: Env): Promise<CryptoKey | undefined> => {
  const secret = env.WORKOS_API_KEY;
  if (!secret) return undefined;
  return crypto.subtle.importKey(
    "raw",
    encoder.encode(`godspeed-admin-session:v1:${secret}`),
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["sign", "verify"]
  );
};

/** Mint a session cookie value for `email` (exported for tests). */
export const signSession = async (env: Env, email: string, nowS = Math.floor(Date.now() / 1000)): Promise<string> => {
  const key = await sessionKey(env);
  if (!key) throw new Error("WORKOS_API_KEY is not configured");
  const payload = b64url(encoder.encode(JSON.stringify({ e: email.toLowerCase(), x: nowS + SESSION_TTL_S })));
  const sig = new Uint8Array(await crypto.subtle.sign("HMAC", key, encoder.encode(payload)));
  return `${payload}.${b64url(sig)}`;
};

/** The admin email a valid, unexpired session cookie names. */
export const verifySession = async (
  env: Env,
  value: string | undefined,
  nowS = Math.floor(Date.now() / 1000)
): Promise<string | undefined> => {
  if (!value) return undefined;
  const [payload, sig] = value.split(".");
  if (!payload || !sig) return undefined;
  const key = await sessionKey(env);
  if (!key) return undefined;
  try {
    const ok = await crypto.subtle.verify("HMAC", key, fromB64url(sig), encoder.encode(payload));
    if (!ok) return undefined;
    const claims = JSON.parse(new TextDecoder().decode(fromB64url(payload))) as { e?: unknown; x?: unknown };
    if (typeof claims.e !== "string" || typeof claims.x !== "number" || claims.x < nowS) return undefined;
    return claims.e;
  } catch {
    return undefined;
  }
};

/** The CSRF token for forms on a session (HMAC of the cookie value). */
export const csrfToken = async (env: Env, sessionValue: string): Promise<string> => {
  const key = await sessionKey(env);
  if (!key) throw new Error("WORKOS_API_KEY is not configured");
  return b64url(new Uint8Array(await crypto.subtle.sign("HMAC", key, encoder.encode(`csrf:${sessionValue}`))));
};

const cookie = (request: Request, name: string): string | undefined => {
  for (const part of (request.headers.get("cookie") ?? "").split(";")) {
    const [k, ...v] = part.trim().split("=");
    if (k === name) return v.join("=");
  }
  return undefined;
};

const setCookie = (name: string, value: string, maxAge: number, path = "/admin"): string =>
  `${name}=${value}; Path=${path}; Max-Age=${maxAge}; HttpOnly; Secure; SameSite=Lax`;

// ---------------------------------------------------------------------------
// Responses
// ---------------------------------------------------------------------------

const SECURITY_HEADERS = {
  "cache-control": "no-store",
  "x-robots-tag": "noindex",
  "x-frame-options": "DENY",
  "referrer-policy": "no-referrer",
  "content-security-policy":
    "default-src 'none'; style-src 'unsafe-inline'; img-src 'self' data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'"
};

const escapeHtml = (s: string): string =>
  s.replaceAll("&", "&amp;").replaceAll("<", "&lt;").replaceAll(">", "&gt;").replaceAll('"', "&quot;");

const json = (value: unknown, status = 200): Response =>
  new Response(JSON.stringify(value), {
    status,
    headers: { "content-type": "application/json", ...SECURITY_HEADERS }
  });

const page = (title: string, body: string, status: number): Response =>
  new Response(
    `<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><meta name="robots" content="noindex"><title>${title}</title>
<style>body{margin:0;min-height:100vh;display:grid;place-items:center;background:#0c0a08;color:#ece6dc;font:15px/1.6 ui-sans-serif,system-ui,sans-serif}main{max-width:30rem;padding:2rem;text-align:center}h1{font-size:1.05rem;color:#f3d9a4;margin:0 0 .5rem}p{color:#a39a8c;margin:.25rem 0}a{color:#f3d9a4}</style></head>
<body><main>${body}</main></body></html>`,
    { status, headers: { "content-type": "text/html; charset=utf-8", ...SECURITY_HEADERS } }
  );

const forbidden = (): Response =>
  page(
    "Godspeed admin",
    `<h1>Not an admin</h1><p>This account can't see the Godspeed dashboard.</p><p><a href="/admin/logout">Sign in with another account</a></p>`,
    403
  );

const redirect = (location: string, cookies: string[] = []): Response => {
  const headers = new Headers({ location, ...SECURITY_HEADERS });
  for (const c of cookies) headers.append("set-cookie", c);
  return new Response(null, { status: 302, headers });
};

/** The AuthKit authorize URL for the browser flow. */
export const authorizeUrl = (env: Env, origin: string, state: string): string => {
  const params = new URLSearchParams({
    client_id: env.WORKOS_CLIENT_ID,
    redirect_uri: `${origin}/admin/callback`,
    response_type: "code",
    provider: "authkit",
    state
  });
  return `https://api.workos.com/user_management/authorize?${params}`;
};

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

/** Handle `/admin*`; `undefined` = not an admin route. `exchangeCode` is a
 * seam for tests (defaults to the WorkOS code exchange). */
export const handleAdminRoute = async (
  request: Request,
  env: Env,
  url: URL,
  exchangeCode: (code: string) => Promise<{ email: string; emailVerified: boolean }> = async (code) => {
    if (!env.WORKOS_API_KEY) throw new WorkOsAuthFailed("workos not configured");
    const { user } = await exchange(env, env.WORKOS_API_KEY, code);
    return { email: user.email, emailVerified: user.emailVerified };
  }
): Promise<Response | undefined> => {
  const path = url.pathname.replace(/\/+$/, "") || "/";
  if (path !== "/admin" && !path.startsWith("/admin/")) return undefined;
  const accessPost = path === "/admin/access" && request.method === "POST";
  if (request.method !== "GET" && request.method !== "HEAD" && !accessPost) return json({ error: "method not allowed" }, 405);

  if (path === "/admin/logout") {
    return redirect("/admin/signed-out", [setCookie(SESSION_COOKIE, "", 0)]);
  }
  if (path === "/admin/signed-out") {
    return page("Godspeed admin", `<h1>Signed out</h1><p><a href="/admin">Sign in again</a></p>`, 200);
  }

  if (path === "/admin/callback") {
    if (env.AUTH_RATE_LIMITER) {
      const ip = request.headers.get("cf-connecting-ip") ?? "unknown";
      try {
        const { success } = await env.AUTH_RATE_LIMITER.limit({ key: `admin:${ip}` });
        if (!success) return json({ error: "rate_limited" }, 429);
      } catch {
        /* never block sign-in on a limiter failure */
      }
    }
    const code = url.searchParams.get("code");
    const state = url.searchParams.get("state");
    const expected = cookie(request, STATE_COOKIE);
    if (!code || !state || !expected || state !== expected) {
      return page("Godspeed admin", `<h1>Sign-in expired</h1><p><a href="/admin">Try again</a></p>`, 400);
    }
    let user: { email: string; emailVerified: boolean };
    try {
      user = await exchangeCode(code);
    } catch {
      return page("Godspeed admin", `<h1>Sign-in failed</h1><p><a href="/admin">Try again</a></p>`, 401);
    }
    const clearState = setCookie(STATE_COOKIE, "", 0);
    if (!user.emailVerified || !isAdmin(env, user.email)) {
      return new Response(forbidden().body, {
        status: 403,
        headers: { "content-type": "text/html; charset=utf-8", "set-cookie": clearState, ...SECURITY_HEADERS }
      });
    }
    const session = await signSession(env, user.email);
    return redirect("/admin", [setCookie(SESSION_COOKIE, session, SESSION_TTL_S), clearState]);
  }

  const sessionValue = cookie(request, SESSION_COOKIE);
  const email = await verifySession(env, sessionValue);
  const api = path === "/admin/api/stats";
  if (!email) {
    if (api) return json({ error: "unauthenticated" }, 401);
    if (accessPost) return json({ error: "unauthenticated" }, 401);
    if (path === "/admin/access") return redirect("/admin");
    if (path !== "/admin") return json({ error: "not_found" }, 404);
    if (!env.WORKOS_API_KEY || !env.WORKOS_CLIENT_ID || env.WORKOS_CLIENT_ID === "unset") {
      return page("Godspeed admin", "<h1>Admin sign-in is not configured</h1>", 503);
    }
    const state = b64url(crypto.getRandomValues(new Uint8Array(24)));
    return redirect(authorizeUrl(env, url.origin, state), [setCookie(STATE_COOKIE, state, STATE_TTL_S)]);
  }
  if (!isAdmin(env, email)) return api ? json({ error: "forbidden" }, 403) : forbidden();
  if (path === "/admin/access") return handleAccess(request, env, url, email, sessionValue!);
  if (!env.TELEMETRY) return api ? json({ error: "telemetry not configured" }, 503) : page("Godspeed admin", "<h1>No telemetry database bound</h1>", 503);

  const range = parseRange(url.searchParams.get("range"));
  if (!api && path !== "/admin") return json({ error: "not_found" }, 404);
  let stats: Awaited<ReturnType<typeof computeStats>>;
  try {
    stats = await computeStats(env, range);
  } catch (e) {
    const detail = e instanceof Error ? e.message : String(e);
    return api
      ? json({ error: "unavailable", detail }, 503)
      : page(
          "Godspeed admin",
          `<h1>Telemetry store unavailable</h1><p>${escapeHtml(detail)}</p><p>On the Workers free plan this is usually the daily Durable Objects limit; it resets at 00:00 UTC.</p>`,
          503
        );
  }
  if (api) return json(stats);
  return new Response(renderDashboard(stats), {
    headers: { "content-type": "text/html; charset=utf-8", ...SECURITY_HEADERS }
  });
};

// ---------------------------------------------------------------------------
// /admin/access
// ---------------------------------------------------------------------------

const accessHeaders = (nonce: string) => ({
  ...SECURITY_HEADERS,
  "content-security-policy": `default-src 'none'; style-src 'unsafe-inline'; script-src 'nonce-${nonce}'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'`
});

const timingSafeEqual = (a: string, b: string): boolean => {
  if (a.length !== b.length) return false;
  let diff = 0;
  for (let i = 0; i < a.length; i++) diff |= a.charCodeAt(i) ^ b.charCodeAt(i);
  return diff === 0;
};

/** The waitlist page and its form actions; the caller is a signed-in admin. */
const handleAccess = async (request: Request, env: Env, url: URL, admin: string, sessionValue: string): Promise<Response> => {
  if (!accessStore(env)) return page("Godspeed admin", "<h1>No access store bound</h1>", 503);
  const csrf = await csrfToken(env, sessionValue);

  if (request.method === "POST") {
    const origin = request.headers.get("origin");
    if (origin && origin !== url.origin) return json({ error: "bad origin" }, 403);
    if (Number(request.headers.get("content-length") ?? "0") > 4096) return json({ error: "too_large" }, 413);
    const bytes = await request.arrayBuffer();
    if (bytes.byteLength > 4096) return json({ error: "too_large" }, 413);
    const text = new TextDecoder().decode(bytes);
    const parsed = parseAccessForm(text);
    if (!parsed.ok) return page("Godspeed admin", `<h1>Couldn't do that</h1><p>${escapeHtml(parsed.error)}</p><p><a href="/admin/access">Back</a></p>`, 400);
    if (!timingSafeEqual(parsed.csrf, csrf)) {
      return page("Godspeed admin", `<h1>That form expired</h1><p><a href="/admin/access">Reload the page</a> and try again.</p>`, 403);
    }
    try {
      await applyAccessAction(env, parsed.action, parsed.email, admin);
    } catch (e) {
      const detail = e instanceof Error ? e.message : String(e);
      return page("Godspeed admin", `<h1>Access store unavailable</h1><p>${escapeHtml(detail)}</p><p><a href="/admin/access">Back</a></p>`, 503);
    }
    const params = new URLSearchParams({ done: parsed.action, email: parsed.email });
    return new Response(null, { status: 303, headers: { location: `/admin/access?${params}`, ...SECURITY_HEADERS } });
  }

  let overview: Awaited<ReturnType<NonNullable<ReturnType<typeof accessStore>>["accessOverview"]>>;
  try {
    overview = await accessStore(env)!.accessOverview();
  } catch (e) {
    const detail = e instanceof Error ? e.message : String(e);
    return page("Godspeed admin", `<h1>Access store unavailable</h1><p>${escapeHtml(detail)}</p>`, 503);
  }
  const doneAction = url.searchParams.get("done");
  const doneEmail = normalizeEmail(url.searchParams.get("email"));
  const done =
    doneAction && (ACCESS_ACTIONS as readonly string[]).includes(doneAction) && doneEmail
      ? { action: doneAction as AccessAction, email: doneEmail }
      : undefined;
  const nonce = b64url(crypto.getRandomValues(new Uint8Array(16)));
  return new Response(renderAccessPage(env, overview, csrf, nonce, done), {
    headers: { "content-type": "text/html; charset=utf-8", ...accessHeaders(nonce) }
  });
};
