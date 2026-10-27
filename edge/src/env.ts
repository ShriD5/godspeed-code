export interface Env {
  SESSION_ROOMS: DurableObjectNamespace;
  DEVICE_ROOMS: DurableObjectNamespace;
  PREVIEW_ROOMS: DurableObjectNamespace;
  /** Per-user workspace registries (`reg1/{orgId}/{userId}`) — the row-table
   * replacement for the Loro workspace doc (docs/registry-sync.md). */
  REGISTRY_ROOMS: DurableObjectNamespace;
  /** chat2 session rooms (`chat2/{chatId}`) — dumb authenticated log relays
   * replacing SessionRoom's loro-aware s2 rooms (docs/chat2-sync.md). */
  CHAT_ROOMS: DurableObjectNamespace;
  BLOBS: R2Bucket;
  /** Release artifacts (headless tarballs, dmgs, latest.txt) served at
   * /releases/* for the curl-install flow. Optional: unbound ⇒ /install.sh
   * answers 404 and /releases/<asset> redirects to GitHub Releases
   * (Godspeed's deployment, see DEPLOY.md). */
  RELEASES?: R2Bucket;
  WORKOS_CLIENT_ID: string;
  /** "workos" (verify AuthKit JWTs) or "dev" (bearer == userId, never prod). */
  AUTH_MODE: string;
  /** Optional overrides for the WorkOS trust anchor. */
  WORKOS_ISSUER?: string;
  WORKOS_JWKS_URL?: string;
  /** WorkOS secret API key (wrangler secret) — powers the absorbed /auth/*
   * routes (code exchange, refresh, orgs). Unset ⇒ those routes answer 501,
   * matching the old apps/server dev-mode behavior. */
  WORKOS_API_KEY?: string;
  /** Invite list (wrangler secret): comma-separated emails and `@domain`
   * entries, case-insensitive. Unset/empty ⇒ open (see allowlist.ts). */
  ALLOWED_EMAILS?: string;
  /** Workers rate limiting binding for /auth/* (per IP). Unbound ⇒ no limit. */
  AUTH_RATE_LIMITER?: RateLimiter;
  /** APNs auth key (contents of AuthKey_XXXX.p8, wrangler secret) and its
   * key id. Unset ⇒ session notifications are decided and logged, not sent. */
  APNS_KEY_P8?: string;
  APNS_KEY_ID?: string;
  /** Apple team id and the app's bundle id (defaults: the Godspeed iOS app). */
  APNS_TEAM_ID?: string;
  APNS_TOPIC?: string;
}

/** The slice of the Workers rate limiting binding the edge uses. */
export interface RateLimiter {
  limit(options: { key: string }): Promise<{ success: boolean }>;
}

/** APNs settings, when push is set up for this deployment. */
export const apnsConfig = (env: Env) =>
  env.APNS_KEY_P8 && env.APNS_KEY_ID
    ? {
        keyP8: env.APNS_KEY_P8,
        keyId: env.APNS_KEY_ID,
        teamId: env.APNS_TEAM_ID ?? "QPUV967B5U",
        topic: env.APNS_TOPIC ?? "site.shrithan.godspeed.ios"
      }
    : undefined;

/** Header the Worker stamps on requests it forwards into DOs after verifying
 * the caller's JWT. DOs trust it blindly — they are only reachable through
 * the Worker (design §2: "DO never sees an unauthenticated frame"). */
export const AUTH_USER_HEADER = "x-zeron-auth-user";

/** Header the Worker stamps on requests forwarded into workspace-doc rooms
 * (`ws/{orgId}`). Membership (JWT org claim == orgId) is enforced at the
 * Worker; the SessionRoom DO sees this and skips its per-chat
 * claim-on-first-join ownership discipline for the room. */
export const ROOM_KIND_HEADER = "x-zeron-room-kind";
