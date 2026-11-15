import { env } from "cloudflare:test";
import { beforeEach, describe, expect, it } from "vitest";
import { csrfToken, handleAdminRoute, signSession } from "../../src/admin";
import { clearUserCache, gateUser, gateWorkOsUser } from "../../src/allowlist";
import type { Env, RateLimiter } from "../../src/env";
import { accessStore, clearAllowedCache, handleWaitlist } from "../../src/waitlist";

// Fake identities only: this repo is public.
const baseEnv = (overrides: Partial<Env> = {}): Env =>
  ({
    WORKOS_CLIENT_ID: "client_test",
    WORKOS_API_KEY: "sk_test",
    AUTH_MODE: "workos",
    ADMIN_EMAILS: "owner@example.com",
    TELEMETRY: env.TELEMETRY,
    // Storage persists across tests here: a fresh store per env.
    ACCESS_STORE: crypto.randomUUID(),
    TELEMETRY_STORE: crypto.randomUUID(),
    ...overrides
  }) as unknown as Env;

const join = (e: Env, body: unknown, headers: Record<string, string> = {}) =>
  handleWaitlist(
    new Request("https://edge.test/waitlist", {
      method: "POST",
      headers: { "content-type": "application/json", origin: "https://godspeed-code.pages.dev", ...headers },
      body: typeof body === "string" ? body : JSON.stringify(body)
    }),
    e
  );

const admin = async (e: Env, path: string, init: RequestInit = {}, email = "owner@example.com") => {
  const session = await signSession(e, email);
  const headers = new Headers(init.headers);
  headers.set("cookie", `gs_admin=${session}`);
  const request = new Request(`https://edge.test${path}`, { ...init, headers });
  return { res: (await handleAdminRoute(request, e, new URL(request.url)))!, session };
};

const act = async (e: Env, action: string, email: string, opts: { csrf?: string; origin?: string } = {}) => {
  const session = await signSession(e, "owner@example.com");
  const csrf = opts.csrf ?? (await csrfToken(e, session));
  const request = new Request("https://edge.test/admin/access", {
    method: "POST",
    headers: {
      cookie: `gs_admin=${session}`,
      "content-type": "application/x-www-form-urlencoded",
      origin: opts.origin ?? "https://edge.test"
    },
    body: new URLSearchParams({ action, email, csrf }).toString()
  });
  return (await handleAdminRoute(request, e, new URL(request.url)))!;
};

beforeEach(() => {
  clearAllowedCache();
  clearUserCache();
});

describe("POST /waitlist", () => {
  it("stores a sign-up once, never revealing duplicates", async () => {
    const e = baseEnv();
    const first = await join(e, { email: "Tester@Example.com", note: "Codex on a Mac mini" });
    expect(first.status).toBe(200);
    expect(await first.json()).toEqual({ ok: true });
    expect(first.headers.get("access-control-allow-origin")).toBe("https://godspeed-code.pages.dev");
    const again = await join(e, { email: "tester@example.com" });
    expect(again.status).toBe(200);
    const overview = await accessStore(e)!.accessOverview();
    expect(overview.waitlist).toHaveLength(1);
    expect(overview.waitlist[0]).toMatchObject({ email: "tester@example.com", note: "Codex on a Mac mini", status: "pending" });
    expect(overview.pending).toBe(1);
  });

  it("keeps emails out of the telemetry instance", async () => {
    const e = baseEnv();
    await join(e, { email: "tester@example.com" });
    const telemetry = e.TELEMETRY!.get(e.TELEMETRY!.idFromName(e.TELEMETRY_STORE!));
    await expect(telemetry.accessOverview()).resolves.toMatchObject({ waitlist: [] });
  });

  it("rejects invalid input with a message", async () => {
    const e = baseEnv();
    const res = await join(e, { email: "not-an-email" });
    expect(res.status).toBe(400);
    expect(((await res.json()) as { message: string }).message).toMatch(/valid email/);
    expect((await join(e, "{")).status).toBe(400);
    expect((await join(e, { email: "a@example.com", note: "x".repeat(3000) })).status).toBe(413);
    expect(
      (
        await handleWaitlist(
          new Request("https://edge.test/waitlist", { method: "POST", headers: { "content-type": "text/plain" }, body: "a" }),
          e
        )
      ).status
    ).toBe(415);
    expect((await handleWaitlist(new Request("https://edge.test/waitlist"), e)).status).toBe(405);
  });

  it("answers bots like everyone else but stores nothing", async () => {
    const e = baseEnv();
    expect((await join(e, { email: "bot@example.com", website: "http://spam.example" })).status).toBe(200);
    expect((await accessStore(e)!.accessOverview()).waitlist).toHaveLength(0);
  });

  it("answers the CORS preflight for the landing page only", async () => {
    const e = baseEnv();
    const ok = await handleWaitlist(
      new Request("https://edge.test/waitlist", { method: "OPTIONS", headers: { origin: "https://godspeed-code.pages.dev" } }),
      e
    );
    expect(ok.status).toBe(204);
    expect(ok.headers.get("access-control-allow-methods")).toContain("POST");
    const evil = await handleWaitlist(
      new Request("https://edge.test/waitlist", { method: "OPTIONS", headers: { origin: "https://evil.example" } }),
      e
    );
    expect(evil.headers.get("access-control-allow-origin")).toBeNull();
  });

  it("takes a plain form post and redirects back to the page", async () => {
    const e = baseEnv();
    const res = await join(e, "email=form%40example.com&note=hi", { "content-type": "application/x-www-form-urlencoded" });
    expect(res.status).toBe(303);
    expect(res.headers.get("location")).toBe("https://godspeed-code.pages.dev/?beta=joined#iphone");
    const bad = await join(e, "email=nope", { "content-type": "application/x-www-form-urlencoded" });
    expect(bad.headers.get("location")).toContain("beta=invalid");
    expect((await accessStore(e)!.accessOverview()).waitlist.map((r) => r.email)).toEqual(["form@example.com"]);
  });

  it("rate-limits per IP", async () => {
    const limiter: RateLimiter = { limit: async () => ({ success: false }) };
    const res = await join(baseEnv({ WAITLIST_RATE_LIMITER: limiter }), { email: "a@example.com" });
    expect(res.status).toBe(429);
  });

  it("answers 503 without a store", async () => {
    expect((await join(baseEnv({ TELEMETRY: undefined }), { email: "a@example.com" })).status).toBe(503);
  });
});

describe("/admin/access", () => {
  it("lists the waitlist for admins only", async () => {
    const e = baseEnv();
    await join(e, { email: "tester@example.com", note: "<script>x</script>" });
    const { res } = await admin(e, "/admin/access");
    expect(res.status).toBe(200);
    const html = await res.text();
    expect(html).toContain("tester@example.com");
    expect(html).toContain("&lt;script&gt;x&lt;/script&gt;");
    expect(html).not.toContain("<script>x</script>");
    expect(res.headers.get("content-security-policy")).toMatch(/script-src 'nonce-[^']+'/);
    expect(res.headers.get("content-security-policy")).toContain("form-action 'self'");
    expect((await admin(e, "/admin/access", {}, "stranger@example.com")).res.status).toBe(403);
    const anon = await handleAdminRoute(new Request("https://edge.test/admin/access"), e, new URL("https://edge.test/admin/access"));
    expect(anon!.status).toBe(302);
  });

  it("approves a waitlisted email, which the gate then admits next to the secret", async () => {
    const e = baseEnv({ ALLOWED_EMAILS: "owner@example.com" });
    await join(e, { email: "tester@example.com" });
    const user = { id: "user_tester", email: "Tester@Example.com", emailVerified: true };
    expect((await gateWorkOsUser(e, user))?.status).toBe(403);

    const res = await act(e, "approve", "tester@example.com");
    expect(res.status).toBe(303);
    expect(res.headers.get("location")).toBe("/admin/access?done=approve&email=tester%40example.com");
    expect(await gateWorkOsUser(e, user)).toBeUndefined();
    // gateUser reads the cached email (remembered above), no WorkOS call.
    expect(await gateUser(e, "user_tester")).toBeUndefined();
    // Secret users are untouched.
    expect(await gateWorkOsUser(e, { id: "user_owner", email: "owner@example.com", emailVerified: true })).toBeUndefined();

    const overview = await accessStore(e)!.accessOverview();
    expect(overview.waitlist[0]).toMatchObject({ status: "approved", decided_by: "owner@example.com" });
    expect(overview.allowed.map((r) => r.email)).toEqual(["tester@example.com"]);

    const { res: pageRes } = await admin(e, "/admin/access?done=approve&email=tester%40example.com");
    const html = await pageRes.text();
    expect(html).toContain("Copy invite message");
    expect(html).toContain("can sign in now");
  });

  it("revokes back to the secret-only list", async () => {
    const e = baseEnv({ ALLOWED_EMAILS: "owner@example.com" });
    await act(e, "allow", "demo@example.com");
    const user = { id: "user_demo", email: "demo@example.com", emailVerified: false };
    expect(await gateWorkOsUser(e, user)).toBeUndefined();
    await act(e, "revoke", "demo@example.com");
    expect((await gateWorkOsUser(e, user))?.status).toBe(403);
  });

  it("dismisses and forgets", async () => {
    const e = baseEnv();
    await join(e, { email: "spam@example.com" });
    await act(e, "dismiss", "spam@example.com");
    expect((await accessStore(e)!.accessOverview()).waitlist[0].status).toBe("dismissed");
    await act(e, "forget", "spam@example.com");
    expect((await accessStore(e)!.accessOverview()).waitlist).toHaveLength(0);
  });

  it("refuses a forged CSRF token or a cross-site origin", async () => {
    const e = baseEnv();
    expect((await act(e, "allow", "x@example.com", { csrf: "forged" })).status).toBe(403);
    expect((await act(e, "allow", "x@example.com", { origin: "https://evil.example" })).status).toBe(403);
    expect((await accessStore(e)!.accessOverview()).allowed).toHaveLength(0);
  });

  it("keeps an edge without the secret open until someone is approved", async () => {
    const e = baseEnv();
    const stranger = { id: "user_s", email: "s@example.com", emailVerified: true };
    expect(await gateWorkOsUser(e, stranger)).toBeUndefined();
    await act(e, "allow", "friend@example.com");
    expect((await gateWorkOsUser(e, stranger))?.status).toBe(403);
    expect(await gateWorkOsUser(e, { id: "user_f", email: "friend@example.com", emailVerified: true })).toBeUndefined();
  });
});
