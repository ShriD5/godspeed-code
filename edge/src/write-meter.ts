/**
 * Durable Object SQLite write accounting. The Workers free plan allows
 * 100,000 rows written per day across the whole account, and every row an
 * INSERT/UPDATE/DELETE touches counts, index entries included. The meter
 * wraps a room's `SqlStorage`, sums each statement's `rowsWritten`, and logs
 * at most one line per room object per minute while it writes, so `wrangler tail`
 * shows which room spends the budget.
 */

export const WRITE_LOG_WINDOW_MS = 60_000;

export class WriteMeter {
  /** Rows written since this object started (tests read this). */
  total = 0;
  /** SQL statements that wrote at least one row. */
  statements = 0;
  private pending = 0;
  private lastLogAt = Number.NEGATIVE_INFINITY;

  constructor(
    private readonly room: string,
    private readonly log: (line: string, fields: Record<string, unknown>) => void = (line, fields) =>
      console.log(line, fields)
  ) {}

  /** Leading-edge: the first write after a quiet minute logs at once. A
   * hibernating room is a fresh object on nearly every wake, so a trailing
   * window would never fill; a busy room still logs at most once a minute. */
  add(rows: number, now = Date.now()): void {
    if (rows <= 0) return;
    this.total += rows;
    this.statements += 1;
    this.pending += rows;
    if (now - this.lastLogAt >= WRITE_LOG_WINDOW_MS) this.report(now);
  }

  /** Log what was written since the last line (no-op when nothing was). */
  report(now = Date.now()): void {
    if (this.pending === 0) return;
    this.log("do rows written", { room: this.room, rows: this.pending, sinceLastLine: this.lastLogAt === Number.NEGATIVE_INFINITY ? null : Math.round((now - this.lastLogAt) / 1000) });
    this.pending = 0;
    this.lastLogAt = now;
  }
  /** A `SqlStorage` that counts what it writes. */
  wrap(sql: SqlStorage): SqlStorage {
    const exec = (query: string, ...bindings: unknown[]) => {
      const cursor = sql.exec(query, ...bindings);
      this.add(cursor.rowsWritten);
      return cursor;
    };
    return new Proxy(sql, {
      get(target, prop) {
        if (prop === "exec") return exec;
        const value = Reflect.get(target, prop);
        return typeof value === "function" ? value.bind(target) : value;
      }
    });
  }
}
