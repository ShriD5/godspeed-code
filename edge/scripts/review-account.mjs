/**
 * Create (or repair) the App Review demo account for the iPhone TestFlight
 * beta: a WorkOS AuthKit user with an email + password, its email marked
 * verified, in its own organization (the iOS app refuses accounts with no
 * organization). The password is generated here and written, with the email,
 * to ~/.config/godspeed/testflight.env (mode 0600) as REVIEW_DEMO_EMAIL /
 * REVIEW_DEMO_PASSWORD, where scripts/ios/asc.py picks them up for the beta
 * review details. Nothing secret is printed.
 *
 *   node scripts/review-account.mjs [email]        # default app-review@godspeed-code.pages.dev
 *   node scripts/review-account.mjs --reset-password
 *
 * Afterwards allow the email on the edge: /admin/access → "Allow an email
 * directly" (the account has no Mac attached, so it signs in to an empty
 * workspace; the review notes say so and point at the sample data).
 *
 * The WorkOS key comes from $WORKOS_API_KEY, else ~/.config/godspeed/workos-api-key,
 * and picks the environment (the one the edge's WORKOS_CLIENT_ID belongs to).
 */
import { chmodSync, existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { randomBytes } from "node:crypto";
import { homedir } from "node:os";
import { dirname, join } from "node:path";

const args = process.argv.slice(2);
const reset = args.includes("--reset-password");
const email = (args.find((a) => !a.startsWith("--")) ?? "app-review@godspeed-code.pages.dev").trim().toLowerCase();
const ORG_NAME = "Godspeed App Review";
const ENV_FILE = process.env.GODSPEED_TESTFLIGHT_ENV ?? join(homedir(), ".config/godspeed/testflight.env");

const key = (process.env.WORKOS_API_KEY ?? readFileSync(join(homedir(), ".config/godspeed/workos-api-key"), "utf8")).trim();

const api = async (method, path, body) => {
  const res = await fetch(`https://api.workos.com${path}`, {
    method,
    headers: { authorization: `Bearer ${key}`, "content-type": "application/json" },
    body: body ? JSON.stringify(body) : undefined
  });
  const text = await res.text();
  const data = text ? JSON.parse(text) : {};
  if (!res.ok) {
    console.error(`WorkOS ${method} ${path.split("?")[0]} failed (${res.status}): ${data.message ?? data.code ?? ""}`);
    process.exit(1);
  }
  return data;
};

/** Generated password: 24 url-safe chars plus a digit and a symbol. */
const newPassword = () => `${randomBytes(18).toString("base64url")}7!`;

const readEnv = () => {
  const out = new Map();
  if (!existsSync(ENV_FILE)) return out;
  for (const line of readFileSync(ENV_FILE, "utf8").split("\n")) {
    const m = line.match(/^([A-Z0-9_]+)=(.*)$/);
    if (m) out.set(m[1], m[2]);
  }
  return out;
};

const writeEnv = (updates) => {
  const env = readEnv();
  for (const [k, v] of Object.entries(updates)) env.set(k, v);
  mkdirSync(dirname(ENV_FILE), { recursive: true, mode: 0o700 });
  writeFileSync(ENV_FILE, [...env].map(([k, v]) => `${k}=${v}`).join("\n") + "\n", { mode: 0o600 });
  chmodSync(ENV_FILE, 0o600);
};

const existing = (await api("GET", `/user_management/users?email=${encodeURIComponent(email)}&limit=1`)).data?.[0];
let user = existing;
let password;
if (!user) {
  password = newPassword();
  user = await api("POST", "/user_management/users", {
    email,
    password,
    email_verified: true,
    first_name: "App",
    last_name: "Review"
  });
  console.error(`created WorkOS user ${user.id}`);
} else {
  console.error(`WorkOS user exists: ${user.id}`);
  const stored = readEnv();
  if (reset || stored.get("REVIEW_DEMO_EMAIL") !== email || !stored.get("REVIEW_DEMO_PASSWORD")) {
    password = newPassword();
    await api("PUT", `/user_management/users/${user.id}`, { password, email_verified: true });
    console.error("set a new password");
  } else if (!user.email_verified) {
    await api("PUT", `/user_management/users/${user.id}`, { email_verified: true });
  }
}

const memberships = (await api("GET", `/user_management/organization_memberships?user_id=${user.id}&limit=10`)).data ?? [];
if (memberships.some((m) => m.status === "active")) {
  console.error(`organization membership exists (${memberships.length})`);
} else {
  const org = await api("POST", "/organizations", { name: ORG_NAME });
  await api("POST", "/user_management/organization_memberships", { user_id: user.id, organization_id: org.id });
  console.error(`created organization ${org.id} (${ORG_NAME}) and added the user`);
}

if (password) writeEnv({ REVIEW_DEMO_EMAIL: email, REVIEW_DEMO_PASSWORD: password });
else writeEnv({ REVIEW_DEMO_EMAIL: email });
console.error(`credentials in ${ENV_FILE.replace(homedir(), "~")} (REVIEW_DEMO_EMAIL, REVIEW_DEMO_PASSWORD)`);
console.error(`next: allow ${email} at /admin/access ("Allow an email directly")`);
