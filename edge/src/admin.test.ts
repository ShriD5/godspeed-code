import { describe, expect, it } from "vitest";
import { handleAdminRoute, parseAdmins, signSession, verifySession } from "./admin";
import { compact, usd, weekOverWeek } from "./admin-page";
import type { Env } from "./env";

const env = {
  WORKOS_CLIENT_ID: "client_test",
  WORKOS_API_KEY: "sk_test_secret",
  AUTH_MODE: "workos",
  ADMIN_EMAILS: "Owner@Example.com, pavan@example.com"
} as unknown as Env;

const get = (path: string, cookie?: string) =>
  new Request(`https://edge.test${path}`, { headers: cookie ? { cookie } : {} });

const route = (request: Request, exchangeCode?: (code: string) => Promise<{ email: string; emailVerified: boolean }>) =>
  handleAdminRoute(request, env, new URL(request.url), exchangeCode);

describe("admin auth", () => {
  it("parses the admin list strictly", () => {
    expect([...parseAdmins(" A@x.com,@domain.com,  b@y.org ,nope")]).toEqual(["a@x.com", "b@y.org"]);
    expect(parseAdmins(undefined).size).toBe(0);
  });

  it("signs and verifies sessions, rejecting tampering and expiry", async () => {
    const value = await signSession(env, "owner@example.com", 1000);
    expect(await verifySession(env, value, 1001)).toBe("owner@example.com");
    expect(await verifySession(env, value, 1000 + 13 * 3600)).toBeUndefined();
    const [payload, sig] = value.split(".");
    const forged = btoa(JSON.stringify({ e: "evil@example.com", x: 9e9 })).replace(/=+$/, "");
    expect(await verifySession(env, `${forged}.${sig}`, 1001)).toBeUndefined();
    expect(await verifySession(env, `${payload}.x${sig}`, 1001)).toBeUndefined();
    const otherKey = { ...env, WORKOS_API_KEY: "different" } as Env;
    expect(await verifySession(otherKey, value, 1001)).toBeUndefined();
  });

  it("sends signed-out visitors to AuthKit and refuses the API", async () => {
    const res = (await route(get("/admin")))!;
    expect(res.status).toBe(302);
    const location = new URL(res.headers.get("location")!);
    expect(location.origin + location.pathname).toBe("https://api.workos.com/user_management/authorize");
    expect(location.searchParams.get("redirect_uri")).toBe("https://edge.test/admin/callback");
    expect(location.searchParams.get("client_id")).toBe("client_test");
    expect(res.headers.get("set-cookie")).toContain(`gs_admin_state=${location.searchParams.get("state")}`);
    expect((await route(get("/admin/api/stats")))!.status).toBe(401);
  });

  it("forbids a signed-in non-admin", async () => {
    const cookie = `gs_admin=${await signSession(env, "stranger@example.com")}`;
    expect((await route(get("/admin", cookie)))!.status).toBe(403);
    expect((await route(get("/admin/api/stats", cookie)))!.status).toBe(403);
  });

  it("locks out an admin removed from the list", async () => {
    const cookie = `gs_admin=${await signSession(env, "pavan@example.com")}`;
    const removed = { ...env, ADMIN_EMAILS: "owner@example.com" } as Env;
    const req = get("/admin/api/stats", cookie);
    expect((await handleAdminRoute(req, removed, new URL(req.url)))!.status).toBe(403);
  });

  it("fails closed with no ADMIN_EMAILS", async () => {
    const open = { ...env, ADMIN_EMAILS: undefined } as Env;
    const cookie = `gs_admin=${await signSession(env, "owner@example.com")}`;
    const req = get("/admin/api/stats", cookie);
    expect((await handleAdminRoute(req, open, new URL(req.url)))!.status).toBe(403);
  });

  it("checks state, then admits only verified admins at the callback", async () => {
    const admin = async () => ({ email: "OWNER@example.com", emailVerified: true });
    expect((await route(get("/admin/callback?code=c&state=s1", "gs_admin_state=s2"), admin))!.status).toBe(400);
    expect((await route(get("/admin/callback?code=c&state=s1"), admin))!.status).toBe(400);

    const ok = (await route(get("/admin/callback?code=c&state=s1", "gs_admin_state=s1"), admin))!;
    expect(ok.status).toBe(302);
    expect(ok.headers.get("location")).toBe("/admin");
    const session = ok.headers.get("set-cookie")!.match(/gs_admin=([^;]+)/)![1];
    expect(await verifySession(env, session)).toBe("owner@example.com");

    const stranger = async () => ({ email: "stranger@example.com", emailVerified: true });
    expect((await route(get("/admin/callback?code=c&state=s1", "gs_admin_state=s1"), stranger))!.status).toBe(403);
    const unverified = async () => ({ email: "pavan@example.com", emailVerified: false });
    expect((await route(get("/admin/callback?code=c&state=s1", "gs_admin_state=s1"), unverified))!.status).toBe(403);
    const failing = async () => {
      throw new Error("bad code");
    };
    expect((await route(get("/admin/callback?code=c&state=s1", "gs_admin_state=s1"), failing))!.status).toBe(401);
  });

  it("ignores other paths", async () => {
    expect(await route(get("/administrator"))).toBeUndefined();
    expect(await route(get("/health"))).toBeUndefined();
  });
});

describe("dashboard formatting", () => {
  it("formats numbers, money and week-over-week", () => {
    expect(compact(950)).toBe("950");
    expect(compact(12_345)).toBe("12.3K");
    expect(compact(4_200_000)).toBe("4.20M");
    expect(usd(3.456)).toBe("$3.46");
    expect(usd(1234.5)).toBe("$1,235");
    expect(weekOverWeek(120, 100)).toBe("▲ +20% w/w");
    expect(weekOverWeek(80, 100)).toBe("▼ -20% w/w");
    expect(weekOverWeek(5, 0)).toBe("new this week");
  });
});
