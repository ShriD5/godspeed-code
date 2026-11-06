/**
 * Operator CLI for /admin/access (the iPhone beta allow-list), for when a
 * browser isn't handy:
 *
 *   node scripts/access.mjs allow app-review@godspeed-code.pages.dev
 *   node scripts/access.mjs approve someone@example.com     # a waitlist entry
 *   node scripts/access.mjs revoke someone@example.com
 *
 * It signs a 10-minute admin session the same way the edge does (HMAC keyed
 * by WORKOS_API_KEY, src/admin.ts) for an ADMIN_EMAILS address, then posts the
 * page's own form with its CSRF token. It can only do what that admin could
 * do in the browser, and the store records the admin as `decided_by`.
 *
 * --as <email> picks the admin (default: the owner, the first user in the
 * WorkOS environment, as in scripts/admins.mjs). --edge <url> picks the edge
 * (default the hosted one). The WorkOS key comes from $WORKOS_API_KEY, else
 * ~/.config/godspeed/workos-api-key.
 */
import { createHmac } from "node:crypto";
import { readFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

const argv = process.argv.slice(2);
const flag = (name) => {
  const i = argv.indexOf(name);
  if (i < 0) return undefined;
  const value = argv[i + 1];
  argv.splice(i, 2);
  return value;
};
const edge = (flag("--edge") ?? "https://godspeed-edge.shrid5.workers.dev").replace(/\/+$/, "");
let admin = flag("--as");
const [action, email] = argv;
if (!["allow", "approve", "dismiss", "revoke", "forget"].includes(action ?? "") || !email) {
  console.error("usage: node scripts/access.mjs allow|approve|dismiss|revoke|forget <email> [--as admin@…] [--edge url]");
  process.exit(2);
}

const key = (process.env.WORKOS_API_KEY ?? readFileSync(join(homedir(), ".config/godspeed/workos-api-key"), "utf8")).trim();

if (!admin) {
  // The owner: the oldest user in the environment.
  const res = await fetch("https://api.workos.com/user_management/users?limit=1&order=asc", {
    headers: { authorization: `Bearer ${key}` }
  });
  if (!res.ok) {
    console.error(`WorkOS users request failed (${res.status})`);
    process.exit(1);
  }
  admin = (await res.json()).data?.[0]?.email;
  if (!admin) {
    console.error("no WorkOS users; pass --as");
    process.exit(1);
  }
}

const b64url = (buf) => Buffer.from(buf).toString("base64url");
const hmac = (data) => createHmac("sha256", `godspeed-admin-session:v1:${key}`).update(data).digest();
const payload = b64url(JSON.stringify({ e: admin.toLowerCase(), x: Math.floor(Date.now() / 1000) + 600 }));
const session = `${payload}.${b64url(hmac(payload))}`;
const csrf = b64url(hmac(`csrf:${session}`));

const res = await fetch(`${edge}/admin/access`, {
  method: "POST",
  redirect: "manual",
  headers: {
    cookie: `gs_admin=${session}`,
    origin: edge,
    "content-type": "application/x-www-form-urlencoded"
  },
  body: new URLSearchParams({ action, email, csrf }).toString()
});
if (res.status === 303) {
  console.error(`${action}: ${email.toLowerCase()} (as the ${argv.includes("--as") ? "given" : "owner"} admin)`);
} else if (res.status === 403) {
  console.error("refused (403): that admin isn't in ADMIN_EMAILS, or the edge uses a different WORKOS_API_KEY");
  process.exit(1);
} else {
  console.error(`failed (${res.status}): ${(await res.text()).replace(/<[^>]+>/g, " ").replace(/\s+/g, " ").slice(0, 200)}`);
  process.exit(1);
}
