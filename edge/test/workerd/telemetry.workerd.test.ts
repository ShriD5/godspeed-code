import { env } from "cloudflare:test";
import { describe, expect, it } from "vitest";
import { handleAdminRoute, signSession } from "../../src/admin";
import type { Stats } from "../../src/admin-stats";
import type { Env } from "../../src/env";
import { addDays, handleTelemetryPing, telemetryStore, utcDay, validatePing } from "../../src/telemetry";

const TODAY = utcDay(new Date());

const baseEnv = (overrides: Partial<Env> = {}): Env =>
  ({
    WORKOS_CLIENT_ID: "client_test",
    WORKOS_API_KEY: "sk_test",
    AUTH_MODE: "workos",
    ADMIN_EMAILS: "owner@example.com",
    TELEMETRY: env.TELEMETRY,
    // Storage persists across tests here: a fresh store per env.
    TELEMETRY_STORE: crypto.randomUUID(),
    ...overrides
  }) as unknown as Env;

const uuid = (n: number) => `00000000-0000-4000-8000-${String(n).padStart(12, "0")}`;

const ping = (n: number, day = TODAY, extra: Record<string, unknown> = {}) => ({
  v: 1,
  install_id: uuid(n),
  day,
  app_version: "0.5.8",
  os: "macos",
  arch: "aarch64",
  first_seen: addDays(day, -1),
  active: true,
  ...extra
});

const usage = (day: string, spend: number, harnesses: Record<string, number> = { "claude-code": 2 }) => ({
  day,
  chats_started: 1,
  turns_finished: 2,
  approvals_answered: 3,
  zoomie_runs: 1,
  harnesses,
  tokens: { input: 100, output: 10, cache_read: 1000, cache_write: 5 },
  spend_usd: spend
});

const post = (e: Env, body: unknown, headers: Record<string, string> = {}) =>
  handleTelemetryPing(
    new Request("https://edge.test/telemetry/v1/ping", {
      method: "POST",
      headers: { "content-type": "application/json", ...headers },
      body: typeof body === "string" ? body : JSON.stringify(body)
    }),
    e
  );

/** Store a ping as if it arrived on its own day (the endpoint only takes
 * today's, give or take one). */
const seed = async (e: Env, body: { day: string }) => {
  const validated = validatePing(JSON.parse(JSON.stringify(body)), new Date(`${body.day}T12:00:00Z`));
  if (!validated.ok) throw new Error(validated.error);
  await telemetryStore(e)!.storePing(validated.row);
  return { status: 204 };
};

const adminGet = async (e: Env, path: string, email = "owner@example.com") => {
  const req = new Request(`https://edge.test${path}`, {
    headers: { cookie: `gs_admin=${await signSession(e, email)}` }
  });
  return (await handleAdminRoute(req, e, new URL(req.url)))!;
};

const seedGitHubCache = async (e: Env) => {
  // Keep the test offline: a fresh cache entry means no GitHub fetch.
  await telemetryStore(e)!.cachePut(
    "github",
    JSON.stringify({ stars: 42, downloads: 1234, manifestDownloads: 99, releases: [{ tag: "v0.5.7", downloads: 300, publishedAt: "" }], fetchedAt: Date.now() }),
    Date.now()
  );
};

describe("telemetry ingest on a real TelemetryStore", () => {
  it("accepts a valid ping, rejects junk, and dedupes retries", async () => {
    const e = baseEnv();
    expect((await post(e, ping(1))).status).toBe(204);
    // A retry of the same install+day upserts (here with usage filled in).
    expect((await post(e, ping(1, TODAY, { usage: usage(addDays(TODAY, -1), 2.5) }))).status).toBe(204);

    expect((await post(e, "not json")).status).toBe(400);
    expect((await post(e, { ...ping(2), ip: "1.2.3.4" })).status).toBe(400);
    expect((await post(e, { ...ping(2), install_id: "nope" })).status).toBe(400);
    expect((await post(e, "x".repeat(5000))).status).toBe(413);
    expect((await post(e, "{}", { "content-length": "999999" })).status).toBe(413);
    const wrongMethod = await handleTelemetryPing(new Request("https://edge.test/telemetry/v1/ping"), e);
    expect(wrongMethod.status).toBe(405);

    await seedGitHubCache(e);
    const stats = (await (await adminGet(e, "/admin/api/stats?range=7")).json()) as Stats;
    expect(stats.installs.total).toBe(1);
    const today = stats.dau.perDay.find((d) => d.day === TODAY);
    expect(today?.value).toBe(1);
    const yesterday = stats.usage.perDay.find((d) => d.day === addDays(TODAY, -1));
    expect(yesterday?.spend).toBe(2.5);
    expect(yesterday?.reporters).toBe(1);
  });

  it("rate limits per IP without storing it", async () => {
    const calls: string[] = [];
    const e = baseEnv({
      TELEMETRY_RATE_LIMITER: {
        limit: async ({ key }) => {
          calls.push(key);
          return { success: calls.length <= 1 };
        }
      }
    });
    const headers = { "cf-connecting-ip": "203.0.113.9" };
    expect((await post(e, ping(3), headers)).status).toBe(204);
    const limited = await post(e, ping(4), headers);
    expect(limited.status).toBe(429);
    expect(limited.headers.get("retry-after")).toBe("60");
    expect(calls).toEqual(["telemetry:203.0.113.9", "telemetry:203.0.113.9"]);
  });

  it("answers 503 when no store is bound", async () => {
    expect((await post(baseEnv({ TELEMETRY: undefined }), ping(5))).status).toBe(503);
  });
});

describe("admin dashboard on seeded data", () => {
  it("computes the tiles", async () => {
    const e = baseEnv();
    const d = (k: number) => addDays(TODAY, -k);
    // Install 10: first seen 8 days ago, active every day since (D1, D7).
    for (let k = 8; k >= 0; k--) {
      expect((await seed(e, ping(10, d(k), { first_seen: d(8), usage: k < 8 ? usage(d(k + 1), 1) : undefined }))).status).toBe(204);
    }
    // Install 11: first seen 8 days ago, never back.
    expect((await seed(e, ping(11, d(8), { first_seen: d(8), app_version: "0.5.7", os: "linux", arch: "x86_64" }))).status).toBe(204);
    // Install 12: a background engine yesterday, reporting codex usage.
    expect((await seed(e, ping(12, d(1), { first_seen: d(1), active: false, usage: undefined }))).status).toBe(204);
    expect((await seed(e, ping(12, d(0), { first_seen: d(1), active: false, usage: usage(d(1), 4, { codex: 5, other: 1 }) }))).status).toBe(204);
    await seedGitHubCache(e);

    const res = await adminGet(e, "/admin/api/stats?range=30");
    expect(res.status).toBe(200);
    const stats = (await res.json()) as Stats;
    expect(stats.range).toBe(30);
    expect(stats.installs.total).toBe(3);
    expect(stats.installs.newYesterday).toBe(1);
    expect(stats.dau.yesterday).toBe(1);
    expect(stats.dau.backgroundYesterday).toBe(1);
    expect(stats.wau.current).toBe(1);
    expect(stats.wau.monthly).toBe(2);
    // Spend for yesterday: install 10 ($1) + install 12 ($4).
    expect(stats.usage.perDay.find((u) => u.day === d(1))?.spend).toBe(5);
    expect(stats.usage.last7.spend).toBe(1 * 7 + 4);
    expect(stats.harnesses).toEqual([
      { key: "claude-code", value: 16 },
      { key: "codex", value: 5 },
      { key: "other", value: 1 }
    ]);
    expect(stats.versions[0]).toEqual({ key: "0.5.8", value: 2 });
    expect(stats.platforms.map((p) => p.key)).toEqual(["macos aarch64"]);
    const cohort = stats.retention.find((c) => c.cohort === d(8));
    expect(cohort).toEqual({ cohort: d(8), size: 2, d1: 0.5, d7: 0.5, d30: null });
    expect(stats.github.stars).toBe(42);

    const page = await adminGet(e, "/admin?range=7");
    expect(page.status).toBe(200);
    expect(page.headers.get("content-type")).toContain("text/html");
    expect(page.headers.get("cache-control")).toBe("no-store");
    const html = await page.text();
    for (const text of ["Total installs", "Daily active installs", "Weekly active installs", "Spend / day", "at API prices", "Version adoption", "Harness mix", "Retention", "GitHub"]) {
      expect(html).toContain(text);
    }
    expect(html).not.toContain("<script");

    expect((await adminGet(e, "/admin", "stranger@example.com")).status).toBe(403);
  });

  it("counts legacy update checks", async () => {
    const e = baseEnv();
    const store = telemetryStore(e)!;
    await store.countUpdateCheck(TODAY);
    await store.countUpdateCheck(TODAY);
    await seedGitHubCache(e);
    const stats = (await (await adminGet(e, "/admin/api/stats?range=7")).json()) as Stats;
    expect(stats.updateChecks.total).toBe(2);
  });
});
