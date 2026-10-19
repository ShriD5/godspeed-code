/**
 * Invite-only gate for the hosted edge.
 *
 * `ALLOWED_EMAILS` (a Worker secret) is a comma-separated, case-insensitive
 * list of emails and `@domain.com` entries. Unset or empty ⇒ the gate is off
 * and the edge behaves as it always did (self-hosted deployments).
 *
 * It is enforced wherever a WorkOS identity becomes usable here:
 *  - `/auth/exchange` and `/auth/refresh` check the user WorkOS returns before
 *    handing out tokens;
 *  - every bearer-authenticated request (HTTP, WebSocket upgrades, `/auth/orgs`)
 *    checks the token's user. This matters because AuthKit tokens can be
 *    minted without this edge (PKCE), and tokens issued before the list was
 *    set stay valid until they expire. Access tokens carry no email, so the
 *    user's email is looked up once with the WorkOS API and cached per
 *    isolate (the decision itself is recomputed against the current list).
 *
 * A rejected user gets 403 `{"error":"invite_only","message":…}`, which the
 * desktop and iOS clients show verbatim. Dev mode (`AUTH_MODE=dev`, bearer =
 * user id, no emails) is never gated.
 */
import type { Env } from "./env";

export const INVITE_ONLY_CODE = "invite_only";
export const INVITE_ONLY_MESSAGE =
  "Godspeed Code cloud sync is invite-only for now. Godspeed Code works fully without it, or you can host your own sync server: see edge/DEPLOY.md.";

export interface Allowlist {
  readonly emails: ReadonlySet<string>;
  /** Lowercased domains without the leading `@`. */
  readonly domains: ReadonlySet<string>;
}

/** Parse the secret; `undefined` = no list configured (gate off). */
export const parseAllowlist = (raw: string | undefined): Allowlist | undefined => {
  const entries = (raw ?? "")
    .split(/[,\s]+/)
    .map((e) => e.trim().toLowerCase())
    .filter((e) => e.length > 0);
  if (entries.length === 0) return undefined;
  const emails = new Set<string>();
  const domains = new Set<string>();
  for (const entry of entries) {
    if (entry.startsWith("@")) {
      if (entry.length > 1) domains.add(entry.slice(1));
    } else {
      emails.add(entry);
    }
  }
  return { emails, domains };
};

/**
 * Exact emails always match (WorkOS emails are unique per environment, so a
 * stranger cannot register one that already exists). Domain entries also
 * require a verified email: otherwise anyone could sign up with an unverified
 * address at the domain.
 */
export const emailAllowed = (list: Allowlist, email: string, emailVerified: boolean): boolean => {
  const normalized = email.trim().toLowerCase();
  if (list.emails.has(normalized)) return true;
  const at = normalized.lastIndexOf("@");
  if (at < 0) return false;
  return emailVerified && list.domains.has(normalized.slice(at + 1));
};

export const inviteOnlyResponse = (): Response =>
  new Response(JSON.stringify({ error: INVITE_ONLY_CODE, message: INVITE_ONLY_MESSAGE }), {
    status: 403,
    headers: { "content-type": "application/json" }
  });

const unavailableResponse = (detail: string): Response =>
  new Response(JSON.stringify({ error: "auth_unavailable", message: detail }), {
    status: 503,
    headers: { "content-type": "application/json", "retry-after": "30" }
  });

// ---------------------------------------------------------------------------
// User email cache (per isolate)
// ---------------------------------------------------------------------------

interface CachedUser {
  readonly email: string;
  readonly emailVerified: boolean;
  readonly at: number;
}

/** Fresh for 10 minutes; older entries are still used if WorkOS is down. */
const USER_TTL_MS = 10 * 60 * 1000;
const USER_CACHE_MAX = 1000;
const userCache = new Map<string, CachedUser>();

/** Record a user WorkOS just returned (exchange/refresh) so the bearer gate
 * needs no lookup for them. */
export const rememberUser = (userId: string, email: string, emailVerified: boolean): void => {
  if (userCache.size >= USER_CACHE_MAX) userCache.clear();
  userCache.set(userId, { email, emailVerified, at: Date.now() });
};

/** Test hook. */
export const clearUserCache = (): void => userCache.clear();

type Lookup =
  | { kind: "user"; email: string; emailVerified: boolean }
  | { kind: "missing" }
  | { kind: "error"; detail: string };

const lookupUser = async (apiKey: string, userId: string): Promise<Lookup> => {
  try {
    const res = await fetch(
      `https://api.workos.com/user_management/users/${encodeURIComponent(userId)}`,
      { headers: { authorization: `Bearer ${apiKey}` } }
    );
    if (res.status === 404) return { kind: "missing" };
    if (!res.ok) return { kind: "error", detail: `WorkOS user lookup failed (${res.status})` };
    const body = (await res.json()) as { email?: unknown; email_verified?: unknown };
    if (typeof body.email !== "string") return { kind: "error", detail: "WorkOS user has no email" };
    return { kind: "user", email: body.email, emailVerified: body.email_verified === true };
  } catch (e) {
    return { kind: "error", detail: `WorkOS user lookup failed: ${String(e)}` };
  }
};

/**
 * Gate an authenticated user id. `undefined` = allowed (or no list
 * configured); otherwise the response to send instead. Lookup failures answer
 * 503, never 401/403: clients sign out on a rejected credential, and an
 * invited user must not be signed out by a WorkOS hiccup.
 */
export const gateUser = async (env: Env, userId: string): Promise<Response | undefined> => {
  if (env.AUTH_MODE === "dev") return undefined;
  const list = parseAllowlist(env.ALLOWED_EMAILS);
  if (!list) return undefined;

  const cached = userCache.get(userId);
  if (cached && Date.now() - cached.at < USER_TTL_MS) {
    return emailAllowed(list, cached.email, cached.emailVerified) ? undefined : inviteOnlyResponse();
  }
  if (!env.WORKOS_API_KEY) {
    return unavailableResponse("invite list is set but WORKOS_API_KEY is not configured");
  }
  const found = await lookupUser(env.WORKOS_API_KEY, userId);
  if (found.kind === "user") {
    rememberUser(userId, found.email, found.emailVerified);
    return emailAllowed(list, found.email, found.emailVerified) ? undefined : inviteOnlyResponse();
  }
  if (found.kind === "missing") {
    userCache.delete(userId);
    return inviteOnlyResponse();
  }
  // WorkOS unreachable: a known user keeps their last decision.
  if (cached) {
    return emailAllowed(list, cached.email, cached.emailVerified) ? undefined : inviteOnlyResponse();
  }
  console.warn("allowlist: user lookup failed", found.detail);
  return unavailableResponse("could not verify your account right now; try again shortly");
};

/** Check a user WorkOS returned from exchange/refresh. `undefined` = allowed. */
export const gateWorkOsUser = (
  env: Env,
  user: { readonly id: string; readonly email: string; readonly emailVerified: boolean }
): Response | undefined => {
  if (env.AUTH_MODE === "dev") return undefined;
  rememberUser(user.id, user.email, user.emailVerified);
  const list = parseAllowlist(env.ALLOWED_EMAILS);
  if (!list) return undefined;
  return emailAllowed(list, user.email, user.emailVerified) ? undefined : inviteOnlyResponse();
};
