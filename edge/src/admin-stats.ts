/**
 * The admin dashboard's numbers (`GET /admin`, `GET /admin/api/stats`),
 * computed from the telemetry tables (migrations/0001_telemetry.sql) plus
 * cached GitHub counts. Days are UTC.
 *
 * Usage figures (spend, tokens, turns, harnesses, zoomies) are keyed by the
 * day they happened (`usage_day`), which an install reports with its next
 * day's ping; the latest day is therefore always still filling in, so the
 * headline figures use 7-day averages of complete days.
 */
import type { Env } from "./env";
import { addDays, telemetryStore, utcDay } from "./telemetry";

export const RANGES = [7, 30, 90] as const;
export type Range = (typeof RANGES)[number];

export const parseRange = (raw: string | null): Range => {
  const n = Number(raw);
  return (RANGES as readonly number[]).includes(n) ? (n as Range) : 30;
};

export interface DayValue {
  day: string;
  value: number;
}

export interface Share {
  key: string;
  value: number;
}

export interface UsageDay {
  day: string;
  spend: number;
  tokens: number;
  inputTokens: number;
  outputTokens: number;
  cacheReadTokens: number;
  cacheWriteTokens: number;
  turns: number;
  chats: number;
  approvals: number;
  zoomieRuns: number;
  reporters: number;
}

export interface RetentionCohort {
  cohort: string;
  size: number;
  d1: number | null;
  d7: number | null;
  d30: number | null;
}

export interface GitHubStats {
  stars: number | null;
  downloads: number | null;
  manifestDownloads: number | null;
  releases: { tag: string; downloads: number; publishedAt: string }[];
  fetchedAt: number | null;
  error?: string;
}

export interface StatsCore {
  range: Range;
  today: string;
  generatedAt: string;
  installs: { total: number; newYesterday: number; newPerDay: DayValue[] };
  dau: { yesterday: number; weekBefore: number; perDay: DayValue[]; backgroundYesterday: number };
  wau: { current: number; monthly: number; perDay: DayValue[] };
  usage: { perDay: UsageDay[]; last7: UsageDay; prev7: UsageDay };
  versions: Share[];
  platforms: Share[];
  harnesses: Share[];
  updateChecks: { perDay: DayValue[]; total: number };
  retention: RetentionCohort[];
}

export interface Stats extends StatsCore {
  github: GitHubStats;
}

/** Every day from `from` to `to` inclusive. */
export const daysBetween = (from: string, to: string): string[] => {
  const out: string[] = [];
  for (let day = from; day <= to; day = addDays(day, 1)) out.push(day);
  return out;
};

const fill = (days: string[], rows: { day: string; value: number }[]): DayValue[] => {
  const byDay = new Map(rows.map((r) => [r.day, Number(r.value) || 0]));
  return days.map((day) => ({ day, value: byDay.get(day) ?? 0 }));
};

const emptyUsage = (day: string): UsageDay => ({
  day,
  spend: 0,
  tokens: 0,
  inputTokens: 0,
  outputTokens: 0,
  cacheReadTokens: 0,
  cacheWriteTokens: 0,
  turns: 0,
  chats: 0,
  approvals: 0,
  zoomieRuns: 0,
  reporters: 0
});

const sumUsage = (rows: UsageDay[], label: string): UsageDay => {
  const total = emptyUsage(label);
  for (const r of rows) {
    total.spend += r.spend;
    total.tokens += r.tokens;
    total.inputTokens += r.inputTokens;
    total.outputTokens += r.outputTokens;
    total.cacheReadTokens += r.cacheReadTokens;
    total.cacheWriteTokens += r.cacheWriteTokens;
    total.turns += r.turns;
    total.chats += r.chats;
    total.approvals += r.approvals;
    total.zoomieRuns += r.zoomieRuns;
    total.reporters += r.reporters;
  }
  return total;
};

/** Every tile computed from the store's SQLite, for `range` days ending
 * `today` (runs inside the TelemetryStore Durable Object). */
export const computeStatsSql = (sql: SqlStorage, range: Range, now = new Date()): StatsCore => {
  const today = utcDay(now);
  const yesterday = addDays(today, -1);
  const from = addDays(today, -(range - 1));
  const days = daysBetween(from, today);
  // Usage needs 14 complete days for the w/w comparison regardless of range.
  const usageFrom = addDays(today, -Math.max(range, 15));
  const versionsFrom = addDays(today, -6);
  const q = <T extends Record<string, SqlStorageValue>>(query: string, ...binds: SqlStorageValue[]): T[] =>
    sql.exec<T>(query, ...binds).toArray();

  const total = q<{ n: number }>("SELECT COUNT(*) AS n FROM installs")[0]?.n ?? 0;
  const newPerDay = q<{ day: string; value: number }>(
    "SELECT first_day AS day, COUNT(*) AS value FROM installs WHERE first_day >= ? GROUP BY first_day",
    from
  );
  const dauRows = q<{ day: string; active: number; background: number }>(
    "SELECT day, SUM(active) AS active, SUM(1 - active) AS background FROM pings WHERE day >= ? GROUP BY day",
    addDays(from, -7)
  );
  const wauRows = q<{ day: string; value: number }>(
    `WITH RECURSIVE d(day) AS (SELECT ? UNION ALL SELECT date(day, '+1 day') FROM d WHERE day < ?)
     SELECT d.day AS day,
       (SELECT COUNT(DISTINCT p.install_id) FROM pings p
        WHERE p.active = 1 AND p.day BETWEEN date(d.day, '-6 days') AND d.day) AS value
     FROM d`,
    from,
    today
  );
  const mau =
    q<{ n: number }>(
      "SELECT COUNT(DISTINCT install_id) AS n FROM pings WHERE active = 1 AND day BETWEEN ? AND ?",
      addDays(yesterday, -29),
      yesterday
    )[0]?.n ?? 0;
  const usageRows = q<{
    day: string;
    spend: number;
    input: number;
    output: number;
    cache_read: number;
    cache_write: number;
    turns: number;
    chats: number;
    approvals: number;
    zoomies: number;
    reporters: number;
  }>(
    `SELECT usage_day AS day, SUM(spend_usd) AS spend, SUM(input_tokens) AS input,
       SUM(output_tokens) AS output, SUM(cache_read_tokens) AS cache_read,
       SUM(cache_write_tokens) AS cache_write, SUM(turns_finished) AS turns,
       SUM(chats_started) AS chats, SUM(approvals_answered) AS approvals,
       SUM(zoomie_runs) AS zoomies, COUNT(*) AS reporters
     FROM pings WHERE usage_day >= ? GROUP BY usage_day`,
    usageFrom
  );
  const versions = q<{ key: string; value: number }>(
    `SELECT app_version AS key, COUNT(DISTINCT install_id) AS value FROM pings
     WHERE day >= ? GROUP BY app_version ORDER BY value DESC, key DESC`,
    versionsFrom
  );
  const platforms = q<{ key: string; value: number }>(
    `SELECT os || ' ' || arch AS key, COUNT(DISTINCT install_id) AS value FROM pings
     WHERE day >= ? GROUP BY os, arch ORDER BY value DESC, key`,
    versionsFrom
  );
  const harnesses = q<{ key: string; value: number }>(
    `SELECT j.key AS key, SUM(j.value) AS value FROM pings, json_each(pings.harnesses) AS j
     WHERE pings.usage_day >= ? GROUP BY j.key ORDER BY value DESC, key`,
    from
  );
  const updates = q<{ day: string; value: number }>("SELECT day, hits AS value FROM update_checks WHERE day >= ?", from);
  const retention = q<{ cohort: string; size: number; d1: number; d7: number; d30: number }>(
    `SELECT c.first_seen AS cohort, COUNT(DISTINCT c.install_id) AS size,
       COUNT(DISTINCT CASE WHEN p.day = date(c.first_seen, '+1 day') THEN p.install_id END) AS d1,
       COUNT(DISTINCT CASE WHEN p.day = date(c.first_seen, '+7 days') THEN p.install_id END) AS d7,
       COUNT(DISTINCT CASE WHEN p.day = date(c.first_seen, '+30 days') THEN p.install_id END) AS d30
     FROM (SELECT DISTINCT install_id, first_seen FROM pings WHERE first_seen BETWEEN ? AND ?) AS c
     LEFT JOIN pings p ON p.install_id = c.install_id
       AND p.day IN (date(c.first_seen, '+1 day'), date(c.first_seen, '+7 days'), date(c.first_seen, '+30 days'))
     GROUP BY c.first_seen ORDER BY c.first_seen`,
    addDays(from, -30),
    yesterday
  );

  const dauByDay = new Map(dauRows.map((r) => [r.day, r]));
  const usageByDay = new Map(
    usageRows.map((r): [string, UsageDay] => {
      const input = Number(r.input) || 0;
      const output = Number(r.output) || 0;
      const cacheRead = Number(r.cache_read) || 0;
      const cacheWrite = Number(r.cache_write) || 0;
      return [
        r.day,
        {
          day: r.day,
          spend: Number(r.spend) || 0,
          tokens: input + output + cacheRead + cacheWrite,
          inputTokens: input,
          outputTokens: output,
          cacheReadTokens: cacheRead,
          cacheWriteTokens: cacheWrite,
          turns: Number(r.turns) || 0,
          chats: Number(r.chats) || 0,
          approvals: Number(r.approvals) || 0,
          zoomieRuns: Number(r.zoomies) || 0,
          reporters: Number(r.reporters) || 0
        }
      ];
    })
  );
  const usageDay = (day: string) => usageByDay.get(day) ?? emptyUsage(day);
  // The last 7 days before today (yesterday's reports keep arriving through
  // today) against the 7 before them.
  const last7 = sumUsage(daysBetween(addDays(today, -7), addDays(today, -1)).map(usageDay), "last7");
  const prev7 = sumUsage(daysBetween(addDays(today, -14), addDays(today, -8)).map(usageDay), "prev7");
  const updatesPerDay = fill(days, updates);
  const cohortDone = (cohort: string, k: number) => addDays(cohort, k) <= yesterday;

  return {
    range,
    today,
    generatedAt: now.toISOString(),
    installs: {
      total: Number(total),
      newYesterday: fill([yesterday], newPerDay)[0]?.value ?? 0,
      newPerDay: fill(days, newPerDay)
    },
    dau: {
      yesterday: Number(dauByDay.get(yesterday)?.active ?? 0),
      weekBefore: Number(dauByDay.get(addDays(yesterday, -7))?.active ?? 0),
      perDay: days.map((day) => ({ day, value: Number(dauByDay.get(day)?.active ?? 0) })),
      backgroundYesterday: Number(dauByDay.get(yesterday)?.background ?? 0)
    },
    wau: {
      current: Number(wauRows.find((r) => r.day === yesterday)?.value ?? 0),
      monthly: Number(mau),
      perDay: fill(days, wauRows)
    },
    usage: { perDay: days.map(usageDay), last7, prev7 },
    versions: versions.map((r) => ({ key: String(r.key), value: Number(r.value) })),
    platforms: platforms.map((r) => ({ key: String(r.key), value: Number(r.value) })),
    harnesses: harnesses.map((r) => ({ key: String(r.key), value: Number(r.value) })),
    updateChecks: { perDay: updatesPerDay, total: updatesPerDay.reduce((n, d) => n + d.value, 0) },
    retention: retention.map((r) => {
      const size = Number(r.size) || 0;
      const rate = (n: number, k: number) => (cohortDone(r.cohort, k) && size > 0 ? Number(n) / size : null);
      return { cohort: r.cohort, size, d1: rate(r.d1, 1), d7: rate(r.d7, 7), d30: rate(r.d30, 30) };
    })
  };
};

/** Every tile for `range` days ending today, GitHub included. */
export const computeStats = async (env: Env, range: Range, now = new Date()): Promise<Stats> => {
  const store = telemetryStore(env);
  if (!store) throw new Error("TELEMETRY is not bound");
  const [core, github] = await Promise.all([store.stats(range, now.toISOString()), githubStats(env, now)]);
  return { ...(core as StatsCore), github };
};

// ---------------------------------------------------------------------------
// GitHub stars and release downloads (server-side, cached for an hour)
// ---------------------------------------------------------------------------

const GITHUB_REPO = "ShriD5/godspeed-code";
const GITHUB_TTL_MS = 60 * 60 * 1000;

interface WireRelease {
  tag_name: string;
  published_at: string | null;
  draft?: boolean;
  assets: { name: string; download_count: number }[];
}

const githubFetch = async (env: Env, path: string): Promise<unknown> => {
  const headers: Record<string, string> = {
    accept: "application/vnd.github+json",
    "user-agent": "godspeed-edge-admin",
    "x-github-api-version": "2022-11-28"
  };
  if (env.GITHUB_TOKEN) headers.authorization = `Bearer ${env.GITHUB_TOKEN}`;
  const res = await fetch(`https://api.github.com${path}`, { headers });
  if (!res.ok) throw new Error(`GitHub ${path} answered ${res.status}`);
  return res.json();
};

const fetchGitHub = async (env: Env, now: Date): Promise<GitHubStats> => {
  const repo = (await githubFetch(env, `/repos/${GITHUB_REPO}`)) as { stargazers_count?: number };
  const releases: WireRelease[] = [];
  for (let page = 1; page <= 3; page++) {
    const batch = (await githubFetch(env, `/repos/${GITHUB_REPO}/releases?per_page=100&page=${page}`)) as WireRelease[];
    releases.push(...batch);
    if (batch.length < 100) break;
  }
  const published = releases.filter((r) => !r.draft);
  let downloads = 0;
  let manifestDownloads = 0;
  const perRelease = published.map((r) => {
    let n = 0;
    for (const asset of r.assets) {
      // manifest.json is the updater's poll, not an install: counted apart.
      if (asset.name === "manifest.json") manifestDownloads += asset.download_count;
      else n += asset.download_count;
    }
    downloads += n;
    return { tag: r.tag_name, downloads: n, publishedAt: r.published_at ?? "" };
  });
  return {
    stars: repo.stargazers_count ?? null,
    downloads,
    manifestDownloads,
    releases: perRelease.slice(0, 8),
    fetchedAt: now.getTime()
  };
};

export const githubStats = async (env: Env, now = new Date()): Promise<GitHubStats> => {
  const store = telemetryStore(env);
  const empty: GitHubStats = { stars: null, downloads: null, manifestDownloads: null, releases: [], fetchedAt: null };
  if (!store) return empty;
  const cached = await store.cacheGet("github");
  if (cached && now.getTime() - cached.fetchedAt < GITHUB_TTL_MS) {
    return JSON.parse(cached.body) as GitHubStats;
  }
  try {
    const fresh = await fetchGitHub(env, now);
    await store.cachePut("github", JSON.stringify(fresh), now.getTime());
    return fresh;
  } catch (e) {
    // Stale beats nothing (shared Cloudflare egress IPs hit GitHub's
    // unauthenticated limit; set GITHUB_TOKEN to avoid it).
    const stale = cached ? (JSON.parse(cached.body) as GitHubStats) : empty;
    return { ...stale, error: e instanceof Error ? e.message : String(e) };
  }
};
