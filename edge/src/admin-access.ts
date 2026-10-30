/**
 * `/admin/access`: the iPhone beta waitlist and the approved-email
 * allow-list, behind the same admin sign-in as the dashboard (admin.ts).
 *
 *   GET  /admin/access   the page
 *   POST /admin/access   form actions: approve (a waitlist email), allow (any
 *                        email, e.g. the App Review demo account), dismiss,
 *                        revoke, forget (delete every trace of an email)
 *
 * Approving writes the email to the `access-v1` store, which the bearer gate
 * (allowlist.ts) reads, so it takes effect within a minute everywhere and
 * needs no redeploy. The edge has no email-sending path, so after approving,
 * the page shows the invite message with a "Copy invite message" button and
 * a mailto: link instead of emailing anyone.
 *
 * Forms carry a CSRF token (HMAC of the session cookie), and POSTs must come
 * from this origin; the session cookie is also SameSite=Lax.
 */
import type { AccessOverview, WaitlistRow } from "./access-store";
import type { Env } from "./env";
import { accessStore, clearAllowedCache, LANDING_URL, normalizeEmail } from "./waitlist";

export const ACCESS_ACTIONS = ["approve", "allow", "dismiss", "revoke", "forget"] as const;
export type AccessAction = (typeof ACCESS_ACTIONS)[number];

const esc = (s: string): string =>
  s
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#39;");

/** The message an approved tester gets (pasted into an email or DM). */
export const inviteMessage = (env: Env, email: string): { subject: string; body: string } => {
  const link = env.TESTFLIGHT_URL?.trim();
  return {
    subject: "You're in: the Godspeed iPhone beta",
    body: [
      "Hi! You're in the Godspeed iPhone beta.",
      "",
      `1. On your iPhone, open ${link || "the TestFlight link (coming in a follow-up message)"} and install Godspeed through TestFlight.`,
      `2. On your Mac, install Godspeed Code (${LANDING_URL}), choose Enable sync in the account menu, and sign in with ${email}.`,
      `3. Open Godspeed on your iPhone and sign in with the same email (${email}). Your Mac's chats show up on the phone; agents keep running on the Mac.`,
      "",
      "No Mac handy? Tap \"Look around with sample data\" on the sign-in screen.",
      "Feedback: take a screenshot in the app to send it through TestFlight, or just reply here."
    ].join("\n")
  };
};

const fmtTime = (ms: number | null): string => (ms ? new Date(ms).toISOString().slice(0, 16).replace("T", " ") : "");

const STATUS_LABEL: Record<WaitlistRow["status"], string> = {
  pending: "Pending",
  approved: "Approved",
  dismissed: "Dismissed",
  revoked: "Revoked"
};

const actionForm = (action: AccessAction, email: string, csrf: string, label: string, kind = ""): string =>
  `<form method="post" action="/admin/access"><input type="hidden" name="csrf" value="${esc(csrf)}"><input type="hidden" name="action" value="${action}"><input type="hidden" name="email" value="${esc(email)}"><button class="${kind}" type="submit">${label}</button></form>`;

const inviteBlock = (env: Env, email: string, id: string): string => {
  const { subject, body } = inviteMessage(env, email);
  const mailto = `mailto:${encodeURIComponent(email)}?subject=${encodeURIComponent(subject)}&body=${encodeURIComponent(body)}`;
  return `<div class="invite"><textarea id="${id}" readonly rows="9">${esc(body)}</textarea>
<div class="row"><button type="button" class="primary" data-copy="${id}">Copy invite message</button><a class="btn" href="${esc(mailto)}">Open in Mail</a></div></div>`;
};

export const renderAccessPage = (
  env: Env,
  overview: AccessOverview,
  csrf: string,
  nonce: string,
  done?: { action: AccessAction; email: string }
): string => {
  const banner = done
    ? done.action === "approve" || done.action === "allow"
      ? `<section class="panel done"><h2>${esc(done.email)} can sign in now</h2><p class="sub">Send them the invite. It takes up to a minute to reach every edge location.</p>${inviteBlock(env, done.email, "invite-done")}</section>`
      : `<section class="panel done"><p>${esc({ dismiss: "Dismissed", revoke: "Revoked", forget: "Deleted" }[done.action] ?? "Done")}: ${esc(done.email)}</p></section>`
    : "";
  const testflight = env.TESTFLIGHT_URL?.trim()
    ? `TestFlight link: <a href="${esc(env.TESTFLIGHT_URL.trim())}">${esc(env.TESTFLIGHT_URL.trim())}</a>`
    : "No TestFlight link set yet (the <code>TESTFLIGHT_URL</code> var); invite messages say it's coming.";

  const waitRows = overview.waitlist
    .map((r) => {
      const actions =
        r.status === "pending"
          ? actionForm("approve", r.email, csrf, "Approve", "primary") + actionForm("dismiss", r.email, csrf, "Dismiss")
          : r.status === "approved"
            ? `<details><summary>Invite</summary>${inviteBlock(env, r.email, `inv-${btoa(r.email).replace(/[^A-Za-z0-9]/g, "")}`)}</details>`
            : actionForm("approve", r.email, csrf, "Approve");
      return `<tr class="${r.status}"><td class="email">${esc(r.email)}</td><td class="note">${esc(r.note)}</td><td>${fmtTime(r.joined_at)}</td><td><span class="pill ${r.status}">${STATUS_LABEL[r.status] ?? esc(r.status)}</span>${r.decided_by ? `<span class="by">${esc(r.decided_by)}</span>` : ""}</td><td class="actions">${actions}</td></tr>`;
    })
    .join("");

  const allowRows = overview.allowed
    .map(
      (r) =>
        `<tr><td class="email">${esc(r.email)}</td><td>${fmtTime(r.added_at)}</td><td>${esc(r.added_by)}</td><td class="actions">${actionForm("revoke", r.email, csrf, "Revoke", "danger")}${actionForm("forget", r.email, csrf, "Delete")}</td></tr>`
    )
    .join("");

  return `<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><meta name="robots" content="noindex">
<title>Godspeed Code · beta access</title>
<style>
:root{--bg:#0c0a08;--card:#17130f;--line:#2a241d;--ink:#ece6dc;--ink2:#b4ab9d;--ink3:#857c70;--gold:#e8b75c;--gold-ink:#f3d9a4;--red:#e26d5a}
*{box-sizing:border-box}
body{margin:0;background:radial-gradient(1200px 600px at 10% -10%,#221a10 0,transparent 60%),var(--bg);color:var(--ink);font:14px/1.5 ui-sans-serif,system-ui,-apple-system,"Segoe UI",sans-serif;min-height:100vh}
main{max-width:1100px;margin:0 auto;padding:28px 24px 48px}
header{display:flex;align-items:center;gap:16px;flex-wrap:wrap;margin-bottom:22px}
header h1{font-size:18px;font-weight:600;margin:0;color:var(--gold-ink)}
header .meta{color:var(--ink3);font-size:12px}
header nav{margin-left:auto;display:flex;gap:14px}
header nav a{color:var(--ink3);font-size:12px}
.panel{background:var(--card);border:1px solid var(--line);border-radius:14px;padding:16px;margin-bottom:14px}
.panel h2{margin:0 0 10px;font-size:11px;font-weight:600;letter-spacing:.09em;text-transform:uppercase;color:var(--ink3)}
.done h2{font-size:15px;text-transform:none;letter-spacing:0;color:var(--gold-ink)}
.sub{color:var(--ink2);font-size:12px;margin:0 0 10px}
table{width:100%;border-collapse:collapse;font-size:13px}
th,td{text-align:left;padding:8px;border-bottom:1px solid var(--line);vertical-align:top}
th{color:var(--ink3);font-weight:600;font-size:11px;text-transform:uppercase;letter-spacing:.06em}
td{color:var(--ink2)}
td.email{color:var(--ink);font-weight:500;word-break:break-all}
td.note{max-width:320px;white-space:pre-wrap;word-break:break-word}
tr.dismissed td,tr.revoked td{opacity:.6}
td.actions{white-space:nowrap}
td.actions form{display:inline}
button,.btn{font:inherit;font-size:12px;border-radius:8px;border:1px solid var(--line);background:#221c15;color:var(--ink);padding:5px 11px;cursor:pointer;text-decoration:none;display:inline-block;margin:0 4px 4px 0}
button.primary{background:var(--gold);border-color:var(--gold);color:#1a1206;font-weight:600}
button.danger{color:var(--red)}
.pill{font-size:11px;padding:2px 8px;border-radius:99px;background:#221c15;color:var(--ink2)}
.pill.pending{color:var(--gold-ink)}
.pill.approved{color:#7fd1a8}
.by{display:block;color:var(--ink3);font-size:11px;margin-top:2px}
.invite textarea{width:100%;background:#0f0c09;color:var(--ink);border:1px solid var(--line);border-radius:10px;padding:10px;font:12px/1.5 ui-monospace,Menlo,monospace;resize:vertical}
.invite .row{margin-top:8px}
form.add{display:flex;gap:8px;flex-wrap:wrap}
form.add input[type=email]{flex:1;min-width:220px;background:#0f0c09;color:var(--ink);border:1px solid var(--line);border-radius:8px;padding:6px 10px;font:inherit}
.empty{color:var(--ink3);font-size:12px}
details summary{cursor:pointer;color:var(--gold-ink);font-size:12px}
code{font-size:12px;color:var(--gold-ink)}
</style></head>
<body><main>
<header><h1>Beta access</h1><span class="meta">${overview.pending} pending · ${overview.allowed.length} approved · ${testflight}</span>
<nav><a href="/admin">Dashboard</a><a href="/admin/logout">Sign out</a></nav></header>
${banner}
<section class="panel"><h2>Waitlist</h2>
${overview.waitlist.length ? `<table><thead><tr><th>Email</th><th>Note</th><th>Joined (UTC)</th><th>Status</th><th></th></tr></thead><tbody>${waitRows}</tbody></table>` : `<p class="empty">Nobody on the waitlist yet. The landing page's "Join the iPhone beta" form posts here.</p>`}
</section>
<section class="panel"><h2>Allow an email directly</h2>
<p class="sub">For someone who isn't on the waitlist (a collaborator, the App Review demo account). Same effect as Approve.</p>
<form class="add" method="post" action="/admin/access"><input type="hidden" name="csrf" value="${esc(csrf)}"><input type="hidden" name="action" value="allow"><input type="email" name="email" required placeholder="name@example.com" aria-label="Email"><button class="primary" type="submit">Allow</button></form>
</section>
<section class="panel"><h2>Approved here</h2>
<p class="sub">These can use cloud sync in addition to the <code>ALLOWED_EMAILS</code> secret (which this page can't read or change). Revoke takes up to a minute to reach every edge location.</p>
${overview.allowed.length ? `<table><thead><tr><th>Email</th><th>Approved (UTC)</th><th>By</th><th></th></tr></thead><tbody>${allowRows}</tbody></table>` : `<p class="empty">No one approved here yet.</p>`}
</section>
</main>
<script nonce="${nonce}">
document.addEventListener("click", async (e) => {
  const b = e.target.closest("button[data-copy]");
  if (!b) return;
  const t = document.getElementById(b.dataset.copy);
  try { await navigator.clipboard.writeText(t.value); } catch { t.select(); document.execCommand("copy"); }
  const label = b.textContent; b.textContent = "Copied"; setTimeout(() => { b.textContent = label; }, 1500);
});
</script>
</body></html>`;
};

export type ParsedAction = { ok: true; action: AccessAction; email: string; csrf: string } | { ok: false; error: string };

export const parseAccessForm = (text: string): ParsedAction => {
  const form = new URLSearchParams(text);
  const action = form.get("action") ?? "";
  if (!(ACCESS_ACTIONS as readonly string[]).includes(action)) return { ok: false, error: "unknown action" };
  const email = normalizeEmail(form.get("email"));
  if (!email) return { ok: false, error: "bad email" };
  return { ok: true, action: action as AccessAction, email, csrf: form.get("csrf") ?? "" };
};

/** Apply one action as `admin`. */
export const applyAccessAction = async (env: Env, action: AccessAction, email: string, admin: string, nowMs = Date.now()): Promise<void> => {
  const store = accessStore(env);
  if (!store) throw new Error("access store not configured");
  if (action === "approve" || action === "allow") await store.accessAllow(email, admin, nowMs);
  else if (action === "revoke") await store.accessRevoke(email, admin, nowMs);
  else if (action === "dismiss") await store.waitlistDismiss(email, admin, nowMs);
  else await store.accessForget(email);
  // This isolate sees the change at once; others within the cache's minute.
  clearAllowedCache();
};
