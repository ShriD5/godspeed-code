/**
 * Beta access: the iPhone waitlist and the approved-email allow-list.
 *
 * Both tables live in their own instance (`access-v1`) of the existing
 * SQLite-backed `TelemetryStore` Durable Object class, NOT in the telemetry
 * instance: the telemetry database holds no emails (docs/PRIVACY.md), and a
 * separate instance is a separate SQLite database. No new class, migration or
 * D1 database is needed (the account's D1 quota is used up).
 *
 *  - `waitlist`: one row per email that asked to join (POST /waitlist), with
 *    an optional note and its status: pending → approved / dismissed /
 *    revoked.
 *  - `allowed_emails`: emails an admin approved on /admin/access. The bearer
 *    gate (allowlist.ts) admits these in addition to the `ALLOWED_EMAILS`
 *    secret, so approving someone needs no redeploy.
 *
 * Functions take the DO's `SqlStorage`; `TelemetryStore` exposes them over
 * RPC (telemetry-store.ts).
 */

export const ACCESS_STORE_NAME = "access-v1";

/** Pending + decided rows kept at most; a flood of sign-ups past this is
 * refused (the endpoint is public). */
export const MAX_WAITLIST_ROWS = 20_000;
/** Rows the admin page lists per section. */
export const LIST_LIMIT = 500;

export type WaitlistStatus = "pending" | "approved" | "dismissed" | "revoked";

export type WaitlistRow = {
  email: string;
  note: string;
  joined_at: number;
  status: WaitlistStatus;
  decided_at: number | null;
  decided_by: string | null;
};

export type AllowedRow = {
  email: string;
  added_at: number;
  added_by: string;
};

export interface AccessOverview {
  waitlist: WaitlistRow[];
  allowed: AllowedRow[];
  pending: number;
}

const SCHEMA = [
  `CREATE TABLE IF NOT EXISTS waitlist (
    email TEXT PRIMARY KEY,
    note TEXT NOT NULL DEFAULT '',
    joined_at INTEGER NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    decided_at INTEGER,
    decided_by TEXT
  ) WITHOUT ROWID`,
  "CREATE INDEX IF NOT EXISTS waitlist_status ON waitlist (status, joined_at)",
  `CREATE TABLE IF NOT EXISTS allowed_emails (
    email TEXT PRIMARY KEY,
    added_at INTEGER NOT NULL,
    added_by TEXT NOT NULL
  ) WITHOUT ROWID`
];

export const ensureAccessSchema = (sql: SqlStorage): void => {
  for (const statement of SCHEMA) sql.exec(statement);
};

export type JoinResult = "added" | "exists" | "full";

/** Add an email to the waitlist. An existing row is left as it is (its status
 * and first note stand), except that a pending row with no note takes one. */
export const waitlistJoin = (sql: SqlStorage, email: string, note: string, nowMs: number): JoinResult => {
  const existing = sql
    .exec<{ status: string; note: string }>("SELECT status, note FROM waitlist WHERE email = ?", email)
    .toArray()[0];
  if (existing) {
    if (existing.status === "pending" && !existing.note && note) {
      sql.exec("UPDATE waitlist SET note = ? WHERE email = ?", note, email);
    }
    return "exists";
  }
  const count = sql.exec<{ n: number }>("SELECT count(*) AS n FROM waitlist").one().n;
  if (count >= MAX_WAITLIST_ROWS) return "full";
  sql.exec("INSERT INTO waitlist (email, note, joined_at, status) VALUES (?, ?, ?, 'pending')", email, note, nowMs);
  return "added";
};

/** Approve (from the waitlist) or allow directly (an email typed on the admin
 * page, e.g. the App Review demo account). */
export const accessAllow = (sql: SqlStorage, email: string, by: string, nowMs: number): void => {
  sql.exec(
    "INSERT INTO allowed_emails (email, added_at, added_by) VALUES (?, ?, ?) ON CONFLICT (email) DO NOTHING",
    email,
    nowMs,
    by
  );
  sql.exec(
    "UPDATE waitlist SET status = 'approved', decided_at = ?, decided_by = ? WHERE email = ?",
    nowMs,
    by,
    email
  );
};

/** Take an approved email off the allow-list (the secret still applies). */
export const accessRevoke = (sql: SqlStorage, email: string, by: string, nowMs: number): void => {
  sql.exec("DELETE FROM allowed_emails WHERE email = ?", email);
  sql.exec(
    "UPDATE waitlist SET status = 'revoked', decided_at = ?, decided_by = ? WHERE email = ? AND status = 'approved'",
    nowMs,
    by,
    email
  );
};

/** Hide a pending request (spam, not now). */
export const waitlistDismiss = (sql: SqlStorage, email: string, by: string, nowMs: number): void => {
  sql.exec(
    "UPDATE waitlist SET status = 'dismissed', decided_at = ?, decided_by = ? WHERE email = ? AND status = 'pending'",
    nowMs,
    by,
    email
  );
};

/** Delete an email's waitlist row and allow-list entry entirely (a removal
 * request; docs/PRIVACY.md). */
export const accessForget = (sql: SqlStorage, email: string): void => {
  sql.exec("DELETE FROM allowed_emails WHERE email = ?", email);
  sql.exec("DELETE FROM waitlist WHERE email = ?", email);
};

export const allowedEmails = (sql: SqlStorage): string[] =>
  sql
    .exec<{ email: string }>("SELECT email FROM allowed_emails")
    .toArray()
    .map((r) => r.email);

/** The admin page's view: pending first (oldest first), then decided rows
 * (newest first), and the allow-list (newest first). */
export const accessOverview = (sql: SqlStorage): AccessOverview => {
  const waitlist = sql
    .exec<WaitlistRow>(
      `SELECT email, note, joined_at, status, decided_at, decided_by FROM waitlist
       ORDER BY status = 'pending' DESC,
                CASE WHEN status = 'pending' THEN joined_at ELSE -coalesce(decided_at, joined_at) END
       LIMIT ?`,
      LIST_LIMIT
    )
    .toArray();
  const allowed = sql
    .exec<AllowedRow>("SELECT email, added_at, added_by FROM allowed_emails ORDER BY added_at DESC LIMIT ?", LIST_LIMIT)
    .toArray();
  const pending = sql.exec<{ n: number }>("SELECT count(*) AS n FROM waitlist WHERE status = 'pending'").one().n;
  return { waitlist, allowed, pending };
};
