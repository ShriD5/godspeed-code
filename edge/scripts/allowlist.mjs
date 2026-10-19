/**
 * Print the ALLOWED_EMAILS value for an invite-only edge: every existing
 * WorkOS user plus any extra emails or `@domain` entries given as arguments.
 * Wrangler cannot read a secret back, so the list is rebuilt each time.
 *
 * The value goes to stdout and only the count goes to stderr, so pipe it
 * straight into the secret and nothing is printed:
 *
 *   node scripts/allowlist.mjs friend@example.com | npx wrangler secret put ALLOWED_EMAILS
 *
 * The WorkOS key comes from $WORKOS_API_KEY, else ~/.config/godspeed/workos-api-key.
 * The users listed are those in the key's environment.
 */
import { readFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

const key = (
  process.env.WORKOS_API_KEY ??
  readFileSync(join(homedir(), ".config/godspeed/workos-api-key"), "utf8")
).trim();

const entries = new Set(
  process.argv
    .slice(2)
    .map((e) => e.trim().toLowerCase())
    .filter(Boolean)
);
let fromWorkOs = 0;
let after;
do {
  const url = new URL("https://api.workos.com/user_management/users");
  url.searchParams.set("limit", "100");
  if (after) url.searchParams.set("after", after);
  const res = await fetch(url, { headers: { authorization: `Bearer ${key}` } });
  if (!res.ok) {
    console.error(`WorkOS users request failed (${res.status})`);
    process.exit(1);
  }
  const page = await res.json();
  for (const user of page.data) {
    if (user.email) {
      entries.add(user.email.trim().toLowerCase());
      fromWorkOs += 1;
    }
  }
  after = page.list_metadata?.after ?? undefined;
} while (after);

if (entries.size === 0) {
  console.error("no emails found; refusing to print an empty list (it would open the edge)");
  process.exit(1);
}
console.error(`${entries.size} entries (${fromWorkOs} WorkOS users, ${process.argv.length - 2} extra)`);
process.stdout.write([...entries].join(","));
