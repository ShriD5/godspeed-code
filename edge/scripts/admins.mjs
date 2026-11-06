/**
 * Print the ADMIN_EMAILS value for the private dashboard (`/admin`): the
 * owner (the first user ever created in the WorkOS environment) plus any
 * extra emails given as arguments. Wrangler cannot read a secret back, so the
 * list is rebuilt each time: always pass every extra admin.
 *
 * The value goes to stdout and only counts go to stderr, so pipe it straight
 * into the secret and nothing is printed:
 *
 *   node scripts/admins.mjs | npx wrangler secret put ADMIN_EMAILS
 *   node scripts/admins.mjs pavan@example.com devakumar@example.com | npx wrangler secret put ADMIN_EMAILS
 *
 * Admins sign in at /admin with that email through AuthKit, and the email
 * must be verified there. The WorkOS key comes from $WORKOS_API_KEY, else
 * ~/.config/godspeed/workos-api-key.
 */
import { readFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

const key = (
  process.env.WORKOS_API_KEY ??
  readFileSync(join(homedir(), ".config/godspeed/workos-api-key"), "utf8")
).trim();

const extras = process.argv
  .slice(2)
  .map((e) => e.trim().toLowerCase())
  .filter(Boolean);
for (const email of extras) {
  if (!/^[^@\s,]+@[^@\s,]+\.[^@\s,]+$/.test(email)) {
    console.error("an argument is not an email address; refusing");
    process.exit(1);
  }
}

const users = [];
let after;
do {
  const url = new URL("https://api.workos.com/user_management/users");
  url.searchParams.set("limit", "100");
  url.searchParams.set("order", "asc");
  if (after) url.searchParams.set("after", after);
  const res = await fetch(url, { headers: { authorization: `Bearer ${key}` } });
  if (!res.ok) {
    console.error(`WorkOS users request failed (${res.status})`);
    process.exit(1);
  }
  const page = await res.json();
  users.push(...page.data);
  after = page.list_metadata?.after ?? undefined;
} while (after);

const owner = users
  .filter((u) => u.email)
  .sort((a, b) => String(a.created_at).localeCompare(String(b.created_at)))[0];
if (!owner) {
  console.error("no WorkOS users found; refusing to print an empty list");
  process.exit(1);
}

const admins = new Set([owner.email.trim().toLowerCase(), ...extras]);
const known = new Map(users.filter((u) => u.email).map((u) => [u.email.trim().toLowerCase(), u]));
const verified = [...admins].filter((e) => known.get(e)?.email_verified === true).length;
console.error(
  `${admins.size} admins (owner + ${admins.size - 1} extra); ${verified} already signed in with a verified email`
);
process.stdout.write([...admins].join(","));
