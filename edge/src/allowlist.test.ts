import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { SignJWT, exportJWK, generateKeyPair } from "jose";
import {
  INVITE_ONLY_MESSAGE,
  clearUserCache,
  emailAllowed,
  gateUser,
  parseAllowlist
} from "./allowlist";
import { handleAuthRoute } from "./auth-routes";
import type { Env, RateLimiter } from "./env";

// Fake identities only: this repo is public.
const OWNER = { id: "user_owner", email: "Owner@Example.com", verified: true };
const TEAM = { id: "user_team", email: "dev@team.example", verified: true };
const UNVERIFIED_TEAM = { id: "user_unverified", email: "new@team.example", verified: false };
const STRANGER = { id: "user_stranger", email: "someone@elsewhere.example", verified: true };
const USERS = [OWNER, TEAM, UNVERIFIED_TEAM, STRANGER];

const CLIENT_ID = "client_test";
const JWKS_URL = "https://jwks.test/keys";
const ISSUER = `https://api.workos.com/user_management/${CLIENT_ID}`;

const env = (overrides: Partial<Env> = {}): Env =>
  ({
    WORKOS_CLIENT_ID: CLIENT_ID,
    AUTH_MODE: "workos",
    WORKOS_API_KEY: "sk_test_fake",
    WORKOS_JWKS_URL: JWKS_URL,
    ...overrides
  }) as Env;

let signToken: (sub: string) => Promise<string>;
let jwks: unknown;

beforeAll(async () => {
  const { privateKey, publicKey } = await generateKeyPair("RS256");
  jwks = { keys: [{ ...(await exportJWK(publicKey)), kid: "k1", alg: "RS256", use: "sig" }] };
  signToken = (sub) =>
    new SignJWT({ sid: "session_1" })
      .setProtectedHeader({ alg: "RS256", kid: "k1" })
      .setSubject(sub)
      .setIssuer(ISSUER)
      .setIssuedAt()
      .setExpirationTime("5m")
      .sign(privateKey);
});

/** Mock WorkOS + JWKS. `lookupStatus` overrides the user-lookup response. */
const mockFetch = (opts: { lookupStatus?: number; authUser?: (typeof USERS)[number] } = {}) => {
  const calls: string[] = [];
  const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = typeof input === "string" ? input : input instanceof URL ? input.href : input.url;
    calls.push(url);
    if (url === JWKS_URL) return Response.json(jwks);
    const wire = (u: (typeof USERS)[number]) => ({
      id: u.id,
      email: u.email,
      email_verified: u.verified,
      first_name: null,
      last_name: null
    });
    if (url.endsWith("/user_management/authenticate") && init?.method === "POST") {
      const u = opts.authUser ?? OWNER;
      return Response.json({ user: wire(u), access_token: "at", refresh_token: "rt" });
    }
    const m = /\/user_management\/users\/([^/?]+)$/.exec(url);
    if (m) {
      if (opts.lookupStatus) return new Response("{}", { status: opts.lookupStatus });
      const u = USERS.find((x) => x.id === decodeURIComponent(m[1]));
      return u ? Response.json(wire(u)) : new Response("{}", { status: 404 });
    }
    if (url.includes("/organization_memberships")) return Response.json({ data: [] });
    return new Response("unexpected", { status: 599 });
  });
  vi.stubGlobal("fetch", fetchMock);
  return calls;
};

afterEach(() => {
  vi.unstubAllGlobals();
  clearUserCache();
});

describe("parseAllowlist / emailAllowed", () => {
  it("treats unset, empty and blank lists as open", () => {
    expect(parseAllowlist(undefined)).toBeUndefined();
    expect(parseAllowlist("")).toBeUndefined();
    expect(parseAllowlist(" , ,")).toBeUndefined();
  });

  it("matches emails case-insensitively", () => {
    const list = parseAllowlist(" owner@example.COM , other@example.org")!;
    expect(emailAllowed(list, "OWNER@example.com", false)).toBe(true);
    expect(emailAllowed(list, "stranger@example.com", true)).toBe(false);
  });

  it("matches @domain entries only for verified emails", () => {
    const list = parseAllowlist("@Team.Example")!;
    expect(emailAllowed(list, "dev@team.example", true)).toBe(true);
    expect(emailAllowed(list, "dev@team.example", false)).toBe(false);
    expect(emailAllowed(list, "dev@sub.team.example", true)).toBe(false);
    expect(emailAllowed(list, "dev@team.example.evil", true)).toBe(false);
  });
});

describe("gateUser (bearer requests)", () => {
  it("is open when ALLOWED_EMAILS is unset, without calling WorkOS", async () => {
    const calls = mockFetch();
    expect(await gateUser(env(), STRANGER.id)).toBeUndefined();
    expect(await gateUser(env({ ALLOWED_EMAILS: "" }), STRANGER.id)).toBeUndefined();
    expect(calls).toHaveLength(0);
  });

  it("allows listed users and caches the lookup", async () => {
    const calls = mockFetch();
    const e = env({ ALLOWED_EMAILS: "owner@example.com" });
    expect(await gateUser(e, OWNER.id)).toBeUndefined();
    expect(await gateUser(e, OWNER.id)).toBeUndefined();
    expect(calls.filter((c) => c.includes("/users/"))).toHaveLength(1);
  });

  it("rejects everyone else with 403 invite_only", async () => {
    mockFetch();
    const res = await gateUser(env({ ALLOWED_EMAILS: "owner@example.com" }), STRANGER.id);
    expect(res?.status).toBe(403);
    expect(await res?.json()).toEqual({ error: "invite_only", message: INVITE_ONLY_MESSAGE });
  });

  it("honours domain entries", async () => {
    mockFetch();
    const e = env({ ALLOWED_EMAILS: "@team.example" });
    expect(await gateUser(e, TEAM.id)).toBeUndefined();
    expect((await gateUser(e, UNVERIFIED_TEAM.id))?.status).toBe(403);
    expect((await gateUser(e, OWNER.id))?.status).toBe(403);
  });

  it("rejects deleted users", async () => {
    mockFetch();
    expect((await gateUser(env({ ALLOWED_EMAILS: "owner@example.com" }), "user_gone"))?.status).toBe(403);
  });

  it("answers 503 (not 401/403) when WorkOS can't be reached", async () => {
    mockFetch({ lookupStatus: 500 });
    const res = await gateUser(env({ ALLOWED_EMAILS: "owner@example.com" }), OWNER.id);
    expect(res?.status).toBe(503);
  });

  it("never gates dev mode", async () => {
    const calls = mockFetch();
    expect(await gateUser(env({ AUTH_MODE: "dev", ALLOWED_EMAILS: "owner@example.com" }), "anyone")).toBeUndefined();
    expect(calls).toHaveLength(0);
  });
});

const post = (path: string, body: unknown) =>
  new Request(`https://edge.test${path}`, {
    method: "POST",
    headers: { "content-type": "application/json", "cf-connecting-ip": "203.0.113.9" },
    body: JSON.stringify(body)
  });

const route = (request: Request, e: Env) => handleAuthRoute(request, e, new URL(request.url));

describe("auth routes", () => {
  it("exchange hands tokens to listed users", async () => {
    mockFetch({ authUser: OWNER });
    const res = await route(post("/auth/exchange", { code: "c" }), env({ ALLOWED_EMAILS: "owner@example.com" }));
    expect(res?.status).toBe(200);
    const body = (await res?.json()) as { user: Record<string, unknown>; accessToken: string };
    expect(body.accessToken).toBe("at");
    expect(body.user).toEqual({ id: OWNER.id, email: OWNER.email, firstName: null, lastName: null });
  });

  it("exchange refuses strangers with invite_only", async () => {
    mockFetch({ authUser: STRANGER });
    const res = await route(post("/auth/exchange", { code: "c" }), env({ ALLOWED_EMAILS: "owner@example.com" }));
    expect(res?.status).toBe(403);
    expect(((await res?.json()) as { error: string }).error).toBe("invite_only");
  });

  it("exchange is open without a list", async () => {
    mockFetch({ authUser: STRANGER });
    const res = await route(post("/auth/exchange", { code: "c" }), env());
    expect(res?.status).toBe(200);
  });

  it("refresh refuses strangers and allows listed users", async () => {
    mockFetch({ authUser: STRANGER });
    const e = env({ ALLOWED_EMAILS: "owner@example.com,@team.example" });
    expect((await route(post("/auth/refresh", { refreshToken: "r" }), e))?.status).toBe(403);
    mockFetch({ authUser: TEAM });
    expect((await route(post("/auth/refresh", { refreshToken: "r" }), e))?.status).toBe(200);
  });

  it("/auth/orgs checks the bearer's user (tokens minted elsewhere)", async () => {
    mockFetch();
    const e = env({ ALLOWED_EMAILS: "owner@example.com" });
    const get = async (sub: string) =>
      route(
        new Request("https://edge.test/auth/orgs", {
          headers: { authorization: `Bearer ${await signToken(sub)}` }
        }),
        e
      );
    expect((await get(OWNER.id))?.status).toBe(200);
    const denied = await get(STRANGER.id);
    expect(denied?.status).toBe(403);
    expect(((await denied?.json()) as { error: string }).error).toBe("invite_only");
  });

  it("serves the invite-only page", async () => {
    const res = await route(new Request("https://edge.test/auth/invite-only"), env());
    expect(res?.status).toBe(403);
    expect(res?.headers.get("content-type")).toContain("text/html");
    expect(await res?.text()).toContain("invite-only for now");
  });

  it("rate limits /auth/* per IP when the binding is present", async () => {
    mockFetch({ authUser: OWNER });
    const keys: string[] = [];
    const limiter: RateLimiter = {
      limit: async ({ key }) => {
        keys.push(key);
        return { success: keys.length <= 1 };
      }
    };
    const e = env({ AUTH_RATE_LIMITER: limiter });
    expect((await route(post("/auth/exchange", { code: "c" }), e))?.status).toBe(200);
    const limited = await route(post("/auth/exchange", { code: "c" }), e);
    expect(limited?.status).toBe(429);
    expect(keys).toEqual(["auth:203.0.113.9", "auth:203.0.113.9"]);
  });
});
