/**
 * The telemetry store: ONE SQLite-backed Durable Object (`telemetry-v1`)
 * holding the anonymous usage pings, per-install first/last days, legacy
 * update-check counts and the dashboard's GitHub cache. Reached only through
 * the Worker (src/telemetry.ts validates every ping first; src/admin.ts
 * gates the stats), over Durable Object RPC.
 *
 * A Durable Object rather than D1: SQLite DOs are on the Workers free plan
 * this edge runs on (the account's D1 quota is used up), and one instance is
 * plenty for at most one ping per install per day.
 *
 * Stored: only the validated ping fields (docs/PRIVACY.md). No IPs, user
 * agents, emails, account ids or free text. Rows older than 400 days are
 * deleted on the first write of each UTC day (`prune`).
 */
import { DurableObject } from "cloudflare:workers";
import { computeStatsSql, type Range, type StatsCore } from "./admin-stats";
import type { Env } from "./env";
import { addDays, RETENTION_DAYS, utcDay, type PingRow } from "./telemetry";

const SCHEMA = [
  // One row per install per UTC day; retries upsert onto the same row.
  `CREATE TABLE IF NOT EXISTS pings (
    install_id TEXT NOT NULL,
    day TEXT NOT NULL,
    v INTEGER NOT NULL,
    app_version TEXT NOT NULL,
    os TEXT NOT NULL,
    arch TEXT NOT NULL,
    first_seen TEXT NOT NULL,
    active INTEGER NOT NULL,
    usage_day TEXT,
    chats_started INTEGER NOT NULL DEFAULT 0,
    turns_finished INTEGER NOT NULL DEFAULT 0,
    approvals_answered INTEGER NOT NULL DEFAULT 0,
    zoomie_runs INTEGER NOT NULL DEFAULT 0,
    harnesses TEXT NOT NULL DEFAULT '{}',
    input_tokens INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens INTEGER NOT NULL DEFAULT 0,
    cache_write_tokens INTEGER NOT NULL DEFAULT 0,
    spend_usd REAL NOT NULL DEFAULT 0,
    PRIMARY KEY (install_id, day)
  ) WITHOUT ROWID`,
  "CREATE INDEX IF NOT EXISTS pings_day ON pings (day, active)",
  "CREATE INDEX IF NOT EXISTS pings_usage_day ON pings (usage_day)",
  "CREATE INDEX IF NOT EXISTS pings_first_seen ON pings (first_seen, install_id)",
  // First and last day each install was heard from (total / new installs).
  `CREATE TABLE IF NOT EXISTS installs (
    install_id TEXT PRIMARY KEY,
    first_day TEXT NOT NULL,
    last_day TEXT NOT NULL
  ) WITHOUT ROWID`,
  "CREATE INDEX IF NOT EXISTS installs_first_day ON installs (first_day)",
  "CREATE INDEX IF NOT EXISTS installs_last_day ON installs (last_day)",
  // Legacy update-feed hits (`/releases/manifest.json`) per UTC day.
  "CREATE TABLE IF NOT EXISTS update_checks (day TEXT PRIMARY KEY, hits INTEGER NOT NULL DEFAULT 0) WITHOUT ROWID",
  // Cached GitHub API answers for the dashboard.
  "CREATE TABLE IF NOT EXISTS cache (key TEXT PRIMARY KEY, body TEXT NOT NULL, fetched_at INTEGER NOT NULL) WITHOUT ROWID"
];

const PING_COLUMNS: (keyof PingRow)[] = [
  "install_id",
  "day",
  "v",
  "app_version",
  "os",
  "arch",
  "first_seen",
  "active",
  "usage_day",
  "chats_started",
  "turns_finished",
  "approvals_answered",
  "zoomie_runs",
  "harnesses",
  "input_tokens",
  "output_tokens",
  "cache_read_tokens",
  "cache_write_tokens",
  "spend_usd"
];

const UPSERT_PING = `INSERT INTO pings (${PING_COLUMNS.join(", ")}) VALUES (${PING_COLUMNS.map(() => "?").join(", ")})
  ON CONFLICT (install_id, day) DO UPDATE SET ${PING_COLUMNS.filter((c) => c !== "install_id" && c !== "day")
    .map((c) => `${c} = excluded.${c}`)
    .join(", ")}`;

export class TelemetryStore extends DurableObject<Env> {
  private readonly sql: SqlStorage;
  /** The UTC day retention last ran (memory: at worst once per wake-up). */
  private prunedOn = "";

  constructor(ctx: DurableObjectState, env: Env) {
    super(ctx, env);
    this.sql = ctx.storage.sql;
    for (const statement of SCHEMA) this.sql.exec(statement);
  }

  /** Upsert one validated ping and its install's first/last day. */
  storePing(row: PingRow): void {
    this.pruneDaily();
    this.ctx.storage.transactionSync(() => {
      this.sql.exec(UPSERT_PING, ...PING_COLUMNS.map((c) => row[c]));
      this.sql.exec(
        `INSERT INTO installs (install_id, first_day, last_day) VALUES (?, ?, ?)
         ON CONFLICT (install_id) DO UPDATE SET
           first_day = min(first_day, excluded.first_day),
           last_day = max(last_day, excluded.last_day)`,
        row.install_id,
        row.day,
        row.day
      );
    });
  }

  countUpdateCheck(day: string): void {
    this.pruneDaily();
    this.sql.exec(
      "INSERT INTO update_checks (day, hits) VALUES (?, 1) ON CONFLICT (day) DO UPDATE SET hits = hits + 1",
      day
    );
  }

  private pruneDaily(now = new Date()): void {
    const today = utcDay(now);
    if (this.prunedOn === today) return;
    this.prunedOn = today;
    this.prune(addDays(today, -RETENTION_DAYS));
  }

  /** Delete everything older than `cutoff` (a UTC day). */
  prune(cutoff: string): void {
    this.ctx.storage.transactionSync(() => {
      this.sql.exec("DELETE FROM pings WHERE day < ?", cutoff);
      this.sql.exec("DELETE FROM installs WHERE last_day < ?", cutoff);
      this.sql.exec("DELETE FROM update_checks WHERE day < ?", cutoff);
    });
  }

  /** The dashboard's numbers (everything but GitHub). */
  stats(range: Range, nowIso: string): StatsCore {
    return computeStatsSql(this.sql, range, new Date(nowIso));
  }

  cacheGet(key: string): { body: string; fetchedAt: number } | null {
    const row = this.sql
      .exec<{ body: string; fetched_at: number }>("SELECT body, fetched_at FROM cache WHERE key = ?", key)
      .toArray()[0];
    return row ? { body: row.body, fetchedAt: row.fetched_at } : null;
  }

  cachePut(key: string, body: string, fetchedAt: number): void {
    this.sql.exec(
      "INSERT INTO cache (key, body, fetched_at) VALUES (?, ?, ?) ON CONFLICT (key) DO UPDATE SET body = excluded.body, fetched_at = excluded.fetched_at",
      key,
      body,
      fetchedAt
    );
  }
}
