/**
 * Durable Object SQLite write accounting. The Workers free plan allows
 * 100,000 rows written per day across the whole account, and every row an
 * INSERT/UPDATE/DELETE touches counts, index entries included. The meter
 * wraps a room's `SqlStorage`, sums each statement's `rowsWritten`, and logs
 * one line per room per minute while the room is writing, so `wrangler tail`
 * shows which room spends the budget.
 */

export const WRITE_LOG_WINDOW_MS = 60_000;

export class WriteMeter {
  /** Rows written since this object started (tests read this). */
  total = 0;
  /** SQL statements that wrote at least one row. */
  statements = 0;
  private windowRows = 0;
  private windowStart = 0;

  constructor(
    private readonly room: string,
    private readonly log: (line: string, fields: Record<string, unknown>) => void = (line, fields) =>
      console.log(line, fields)
  ) {}

  add(rows: number, now = Date.now()): void {
    if (rows <= 0) return;
    this.total += rows;
    this.statements += 1;
    if (this.windowStart === 0) this.windowStart = now;
    this.windowRows += rows;
    if (now - this.windowStart >= WRITE_LOG_WINDOW_MS) this.report(now);
  }

  /** Log and reset the current window (no-op when nothing was written). */
  report(now = Date.now()): void {
    if (this.windowRows > 0) {
      this.log("do rows written", {
        room: this.room,
        rows: this.windowRows,
        seconds: Math.round((now - this.windowStart) / 1000)
      });
    }
    this.windowRows = 0;
    this.windowStart = 0;
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
