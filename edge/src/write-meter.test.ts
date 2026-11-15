import { describe, expect, it } from "vitest";
import { PushOutcomes, PERSIST_EVERY_MS } from "./push-outcomes";
import { WRITE_LOG_WINDOW_MS, WriteMeter } from "./write-meter";

describe("WriteMeter", () => {
  it("logs the first write's event at once, then at most one line per minute", async () => {
    const lines: Array<Record<string, unknown>> = [];
    const meter = new WriteMeter("chat2 abc", (_line, fields) => lines.push(fields));
    meter.add(0, 1000);
    expect(lines).toEqual([]);
    meter.add(2, 1000);
    meter.add(1, 1000); // same event: folded into the line
    expect(lines).toEqual([]);
    await Promise.resolve();
    expect(lines).toEqual([{ room: "chat2 abc", rows: 3, sinceLastLine: null }]);
    meter.add(3, 1000 + WRITE_LOG_WINDOW_MS / 2);
    expect(lines.length).toBe(1);
    meter.add(1, 1000 + WRITE_LOG_WINDOW_MS);
    await Promise.resolve();
    expect(lines[1]).toEqual({ room: "chat2 abc", rows: 4, sinceLastLine: 60 });
    expect(meter.total).toBe(7);
    expect(meter.statements).toBe(4);
    meter.report();
    expect(lines.length).toBe(2);
  });
});

describe("PushOutcomes", () => {
  it("counts every push but writes at most once per interval", () => {
    const store = new Map<string, string>();
    const writes: string[] = [];
    const outcomes = new PushOutcomes(
      (key) => store.get(key),
      (key, value) => {
        writes.push(key);
        store.set(key, value);
      }
    );
    const t0 = 1_000_000;
    for (let i = 0; i < 500; i++) outcomes.record("host", i % 10 !== 0, t0 + i * 100);
    // 50 s of pushes: the first record writes, the rest wait out the minute.
    expect(writes.length).toBe(1);
    outcomes.record("phone", true, t0 + PERSIST_EVERY_MS + 1);
    expect(writes.length).toBe(2);
    expect(outcomes.snapshot()).toEqual({
      host: { ok: 450, rejected: 50, lastOkAt: t0 + 499 * 100 },
      phone: { ok: 1, rejected: 0, lastOkAt: t0 + PERSIST_EVERY_MS + 1 }
    });
    // A fresh object (after eviction) resumes from what was stored.
    const reloaded = new PushOutcomes((key) => store.get(key), () => writes.push("x"));
    reloaded.record("host", true, t0 + PERSIST_EVERY_MS + 2);
    expect(writes.length).toBe(2);
    expect(reloaded.snapshot().host!.ok).toBe(451);
  });

  it("reads the pre-throttle key once", () => {
    const store = new Map([["pushOutcomes", JSON.stringify({ old: { ok: 3, rejected: 1, lastOkAt: 5 } })]]);
    const outcomes = new PushOutcomes((key) => store.get(key), (key, value) => store.set(key, value));
    expect(outcomes.snapshot()).toEqual({ old: { ok: 3, rejected: 1, lastOkAt: 5 } });
  });
});
