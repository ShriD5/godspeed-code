import { describe, expect, it } from "vitest";
import { inviteMessage, parseAccessForm } from "./admin-access";
import type { Env } from "./env";
import { corsOrigin, normalizeEmail, validateWaitlist } from "./waitlist";

describe("waitlist validation", () => {
  it("normalizes emails and rejects junk", () => {
    expect(normalizeEmail("  Someone@Example.COM ")).toBe("someone@example.com");
    expect(normalizeEmail("a.b+beta@sub.example.co")).toBe("a.b+beta@sub.example.co");
    for (const bad of ["", "nope", "a@b", "a@@b.com", "a b@c.com", "<x>@y.com", ".a@b.com", "a..b@c.com", "a.@b.com", "a@-b.com", 42, null]) {
      expect(normalizeEmail(bad)).toBeUndefined();
    }
    expect(normalizeEmail(`${"a".repeat(65)}@example.com`)).toBeUndefined();
  });

  it("accepts an email with an optional note, strictly", () => {
    expect(validateWaitlist({ email: "x@example.com" })).toEqual({ ok: true, email: "x@example.com", note: "", bot: false });
    expect(validateWaitlist({ email: "x@example.com", note: " I use Codex\u0007 daily " })).toEqual({
      ok: true,
      email: "x@example.com",
      note: "I use Codex  daily",
      bot: false
    });
    expect(validateWaitlist({ email: "x@example.com", note: "n".repeat(501) }).ok).toBe(false);
    expect(validateWaitlist({ email: "x@example.com", extra: 1 }).ok).toBe(false);
    expect(validateWaitlist({ email: "x@example.com", note: 5 }).ok).toBe(false);
    expect(validateWaitlist({ email: "bad" }).ok).toBe(false);
    expect(validateWaitlist([]).ok).toBe(false);
  });

  it("flags the honeypot", () => {
    expect(validateWaitlist({ email: "x@example.com", website: "http://spam" })).toMatchObject({ ok: true, bot: true });
    expect(validateWaitlist({ email: "x@example.com", website: "" })).toMatchObject({ ok: true, bot: false });
  });

  it("allows CORS only from the landing page and local previews", () => {
    expect(corsOrigin("https://godspeed-code.pages.dev")).toBe("https://godspeed-code.pages.dev");
    expect(corsOrigin("https://abc123.godspeed-code.pages.dev")).toBe("https://abc123.godspeed-code.pages.dev");
    expect(corsOrigin("http://localhost:8788")).toBe("http://localhost:8788");
    expect(corsOrigin("https://shrithan.site")).toBe("https://shrithan.site");
    expect(corsOrigin("https://evil.example")).toBeUndefined();
    expect(corsOrigin("https://godspeed-code.pages.dev.evil.example")).toBeUndefined();
    expect(corsOrigin("http://godspeed-code.pages.dev")).toBeUndefined();
    expect(corsOrigin(null)).toBeUndefined();
    expect(corsOrigin("not a url")).toBeUndefined();
  });
});

describe("admin access forms", () => {
  it("parses actions and normalizes the email", () => {
    expect(parseAccessForm("action=approve&email=A%40Example.com&csrf=t")).toEqual({
      ok: true,
      action: "approve",
      email: "a@example.com",
      csrf: "t"
    });
    expect(parseAccessForm("action=drop&email=a%40example.com").ok).toBe(false);
    expect(parseAccessForm("action=allow&email=nope").ok).toBe(false);
  });

  it("writes an invite with the TestFlight link when set", () => {
    const withLink = inviteMessage({ TESTFLIGHT_URL: "https://testflight.apple.com/join/ABC" } as Env, "a@example.com");
    expect(withLink.body).toContain("https://testflight.apple.com/join/ABC");
    expect(withLink.body).toContain("sign in with a@example.com");
    const without = inviteMessage({} as Env, "a@example.com");
    expect(without.body).toContain("coming in a follow-up message");
  });
});
