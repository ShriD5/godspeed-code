import { describe, expect, it } from "vitest";
import { MAX_PING_BYTES, addDays, validatePing } from "./telemetry";

const NOW = new Date("2026-10-27T12:00:00Z");

export const samplePing = (overrides: Record<string, unknown> = {}) => ({
  v: 1,
  install_id: "4f9d7c1e-2b6a-4c1d-9e3f-0a1b2c3d4e5f",
  day: "2026-10-27",
  app_version: "0.5.8",
  os: "macos",
  arch: "aarch64",
  first_seen: "2026-10-20",
  active: true,
  usage: {
    day: "2026-10-26",
    chats_started: 3,
    turns_finished: 12,
    approvals_answered: 4,
    zoomie_runs: 1,
    harnesses: { "claude-code": 9, codex: 3 },
    tokens: { input: 1000, output: 200, cache_read: 50000, cache_write: 300 },
    spend_usd: 1.23456
  },
  ...overrides
});

describe("validatePing", () => {
  it("accepts the documented payload and flattens it to a row", () => {
    const result = validatePing(samplePing(), NOW);
    expect(result.ok).toBe(true);
    if (!result.ok) return;
    expect(result.row).toMatchObject({
      install_id: "4f9d7c1e-2b6a-4c1d-9e3f-0a1b2c3d4e5f",
      day: "2026-10-27",
      active: 1,
      usage_day: "2026-10-26",
      turns_finished: 12,
      harnesses: '{"claude-code":9,"codex":3}',
      cache_read_tokens: 50000,
      spend_usd: 1.2346
    });
  });

  it("accepts a first ping with no usage", () => {
    const { usage: _usage, ...first } = samplePing();
    const result = validatePing(first, NOW);
    expect(result.ok && result.row.usage_day).toBe(null);
  });

  it.each([
    ["an unknown top-level field", samplePing({ email: "a@b.c" })],
    ["an unknown usage field", samplePing({ usage: { ...samplePing().usage, prompt: "hi" } })],
    ["an unknown token field", samplePing({ usage: { ...samplePing().usage, tokens: { input: 1, output: 1, cache_read: 1, cache_write: 1, reasoning: 1 } } })],
    ["a missing field", (() => { const { os: _os, ...rest } = samplePing(); return rest; })()],
    ["another version", samplePing({ v: 2 })],
    ["a non-v4 install id", samplePing({ install_id: "not-a-uuid" })],
    ["a free-text harness id", samplePing({ usage: { ...samplePing().usage, harnesses: { "/Users/me/repo": 1 } } })],
    ["a day far from now", samplePing({ day: "2026-09-01" })],
    ["first_seen after day", samplePing({ first_seen: "2026-10-28", day: "2026-10-27" })],
    ["usage on the same day", samplePing({ usage: { ...samplePing().usage, day: "2026-10-27" } })],
    ["a negative count", samplePing({ usage: { ...samplePing().usage, turns_finished: -1 } })],
    ["a fractional count", samplePing({ usage: { ...samplePing().usage, chats_started: 1.5 } })],
    ["a string spend", samplePing({ usage: { ...samplePing().usage, spend_usd: "1" } })],
    ["a free-text os", samplePing({ os: "macOS 27.0 (Shri's MacBook)" })],
    ["a long version", samplePing({ app_version: "0.5.8-" + "x".repeat(60) })],
    ["an impossible date", samplePing({ day: "2026-02-31" })],
    ["an array", [samplePing()]],
    ["a string active", samplePing({ active: "true" })]
  ])("rejects %s", (_name, body) => {
    expect(validatePing(body, NOW).ok).toBe(false);
  });

  it("fits comfortably under the size cap", () => {
    expect(JSON.stringify(samplePing()).length).toBeLessThan(MAX_PING_BYTES / 4);
  });

  it("adds days across month ends", () => {
    expect(addDays("2026-10-31", 1)).toBe("2026-11-01");
    expect(addDays("2026-03-01", -1)).toBe("2026-02-28");
  });
});
