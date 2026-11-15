/**
 * Per-device push attribution for `/stats` (kept from the 2026-08-05 incident
 * tooling). It used to cost one meta write per push, as much as the data
 * itself; now counts live in memory and reach storage at most once per
 * `PERSIST_EVERY_MS`, so they are approximate across evictions (diagnostics,
 * not data).
 */

export interface PushOutcome {
  ok: number;
  rejected: number;
  lastOkAt: number;
}

export const PERSIST_EVERY_MS = 60_000;
/** Stored as `{ at, outcomes }`; `pushOutcomes` is the pre-throttle key. */
const KEY = "pushOutcomes2";
const LEGACY_KEY = "pushOutcomes";

type Read = (key: string) => string | undefined;
type Write = (key: string, value: string) => void;

export class PushOutcomes {
  private outcomes?: Record<string, PushOutcome>;
  private persistedAt = 0;
  private dirty = false;

  constructor(
    private readonly read: Read,
    private readonly write: Write
  ) {}

  record(device: string, ok: boolean, now = Date.now()): void {
    const outcomes = this.load();
    const key = device === "" ? "(unknown)" : device;
    const entry = outcomes[key] ?? { ok: 0, rejected: 0, lastOkAt: 0 };
    if (ok) {
      entry.ok += 1;
      entry.lastOkAt = now;
    } else {
      entry.rejected += 1;
    }
    outcomes[key] = entry;
    this.dirty = true;
    if (now - this.persistedAt >= PERSIST_EVERY_MS) this.persist(now);
  }

  persist(now = Date.now()): void {
    if (!this.dirty || !this.outcomes) return;
    this.write(KEY, JSON.stringify({ at: now, outcomes: this.outcomes }));
    this.persistedAt = now;
    this.dirty = false;
  }

  snapshot(): Record<string, PushOutcome> {
    return { ...this.load() };
  }

  /** Forget the cached counts (after an operator wipe of meta). */
  reset(): void {
    this.outcomes = undefined;
    this.persistedAt = 0;
    this.dirty = false;
  }

  private load(): Record<string, PushOutcome> {
    if (this.outcomes) return this.outcomes;
    try {
      const stored = this.read(KEY);
      if (stored) {
        const parsed = JSON.parse(stored) as { at: number; outcomes: Record<string, PushOutcome> };
        this.persistedAt = parsed.at;
        this.outcomes = parsed.outcomes;
        return this.outcomes;
      }
      this.outcomes = JSON.parse(this.read(LEGACY_KEY) ?? "{}") as Record<string, PushOutcome>;
    } catch {
      this.outcomes = {};
    }
    return this.outcomes;
  }
}
