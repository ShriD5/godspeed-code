# Privacy

Last updated: 30 October 2026.

Godspeed Code is a free, open-source app for running coding agents: the Mac (and Linux) app, and an
iPhone companion that follows the Mac's chats. It is made by Shrithan and Devakumar. This page
covers everything the apps and our server receive:

- [The anonymous usage ping](#the-anonymous-usage-ping) the desktop app sends once a day (on by
  default, easy to turn off).
- [Cloud sync and the iPhone app](#cloud-sync-and-the-iphone-app): what is stored when you turn on
  sync, which the iPhone app needs.
- [The iPhone beta waitlist](#the-iphone-beta-waitlist): the email you leave on the website.

We don't sell or share data, show ads, or use third-party analytics or tracking SDKs in any app.

## The anonymous usage ping

Godspeed Code sends one small, anonymous usage ping a day so we know the app is working and
roughly how it is used. It contains no prompts, no code, no file paths, no repo or project names,
no emails, no account or device ids, and no free text of any kind. It is on by default and easy to
turn off.

### What is sent

At most once per UTC day, and only on a day you use Godspeed, the app sends this JSON to
`https://godspeed-edge.shrid5.workers.dev/telemetry/v1/ping` (or `$GODSPEED_EDGE_URL` if you set
one). This is the complete list of fields. The edge rejects anything else, and a test in
`crates/proto/src/telemetry.rs` fails if a field is added without updating this page.

| Field | Example | What it is |
| --- | --- | --- |
| `v` | `1` | Payload version |
| `install_id` | `4f9d7c1e-…` | A random UUID made for this ping only, stored in `telemetry.json` in the data dir (`~/.godspeed`). It is not your device id and is never linked to an account |
| `day` | `2026-10-27` | The UTC day the ping is sent |
| `app_version` | `0.5.8` | Godspeed's version |
| `os` | `macos` | `macos`, `linux`, `windows` or `other` |
| `arch` | `aarch64` | `aarch64`, `x86_64` or `other` |
| `first_seen` | `2026-10-20` | The UTC day this install first ran with the ping |
| `active` | `true` | `true` if a person used Godspeed that day (the desktop app was open or a turn was started), `false` for a background engine that only ran |
| `usage` | | Counts for your most recent earlier day of use not yet reported (a day is only complete once it's over). Missing on the very first ping |
| `usage.day` | `2026-10-26` | The UTC day the counts cover |
| `usage.chats_started` | `3` | Chats created on this install |
| `usage.turns_finished` | `12` | Agent turns that finished |
| `usage.approvals_answered` | `4` | Approvals and agent questions you answered |
| `usage.zoomie_runs` | `1` | Zoomie (always-on agent) runs started |
| `usage.harnesses` | `{"claude-code": 9, "codex": 3}` | Finished turns per agent. Only Godspeed's own agent ids (`claude-code`, `codex`, `cursor`, `devin`, `grok`, `hermes`, `pi`, `muse`, `opencode`, `antigravity`, `mock`, `unknown`); anything else is counted as `other` |
| `usage.tokens` | `{"input": …, "output": …, "cache_read": …, "cache_write": …}` | Token totals for those turns |
| `usage.spend_usd` | `1.25` | What those tokens would cost at list API prices (an estimate; subscription logins pay nothing per token) |

The usage numbers come from the same local index as Settings → Usage, limited to turns Godspeed
ran. Claude Code and Codex history from outside Godspeed is never read for the ping. Model names
are not sent.

### When it is sent

- On launch, about a minute in (five minutes on the very first launch, so you can read the notice
  and turn it off first), if today's ping hasn't gone out yet.
- Otherwise at the day's first activity (a turn started or an approval answered), for an app or
  background engine that stayed open past midnight. An idle day sends nothing.
- A failed send is retried a few times over about 45 minutes, then dropped. It never blocks or
  slows anything.

### What the server keeps

The edge (`edge/src/telemetry.ts`, a Cloudflare Worker) checks every field and stores the ping as
one row per install per day (a retry replaces the row). It does not store your IP address, user
agent or any request header. Your IP is used only, in memory, to rate-limit requests. Rows are kept
for **400 days**, then deleted.

The numbers are shown on a private dashboard that only the maintainers can open (signed in, by
an email allowlist). Nothing is sold or shared.

### How to turn it off

Any one of these stops the ping:

- **Settings → General → Privacy → Share anonymous usage stats** (switch it off), or **Turn off**
  on the first-launch notice.
- `GODSPEED_TELEMETRY=0` in the environment.
- `DO_NOT_TRACK=1` in the environment.

When it is off nothing is sent at all, and nothing about your usage is recorded locally for it.
Turning it off does not delete pings already sent; they are anonymous and age out after 400 days.
The headless engine (`godspeed headless`) honors the same file and variables, and logs a one-line
notice at startup.

### Other connections

Separately from this ping, Godspeed checks GitHub Releases for updates (Settings → General →
Updates turns that off), talks to the agents' own providers when you run them, and connects to the
sync edge only if you turn on sync (next section).

## Cloud sync and the iPhone app

Sync is off by default: without it, chats stay on your Mac and no account exists. It is
invite-only on our hosted server. If you turn it on (**Enable sync** in the Mac app's account
menu) or sign in on the iPhone app, this is what happens.

### What is stored, and where

| Data | Why | Where |
| --- | --- | --- |
| Your account: email address, name (if you gave one), a user id and organization id | Signing in, and keeping your chats yours | [WorkOS](https://workos.com) (sign-in provider) and our server |
| Your chats: messages you send, agent replies, tool output and diffs, chat and project names, and images or files you attach | Showing the same chats on every device you sign in on | Our server: a Cloudflare Worker with Durable Objects and R2 storage |
| Your devices: a random device id the app makes, the device's name (for example "Shrithan's iPhone"), platform and app version | Showing which machine runs which chat, and routing commands to it | Our server |
| Notification settings and the iPhone's push token, if you allow notifications | Telling you when a chat finishes or needs you | Our server; the notification (the chat's title and "Finished" / "Needs your input") goes through Apple's push service |

Data travels over TLS (HTTPS and secure WebSockets) and Cloudflare encrypts it at rest. It is not
end-to-end encrypted: the server can read synced chats, and only the maintainers can access it, to
run and fix the service. Chats you sync stay until you delete them or your account.

### On the iPhone

- **The app is a remote.** Agents run on your Mac (or your own server); the phone shows their chats
  and sends your messages and approvals to them through the sync server.
- **Microphone and speech recognition** (only when you use them): dictation turns speech into
  text with Apple's speech recognition. A Codex voice call you start connects the phone's audio to
  OpenAI's realtime voice service; your Mac sets the call up with the ChatGPT account signed in to
  Codex there. Only the call setup passes through our server; the audio doesn't, and nothing of it
  is stored there.
- **Camera and photos** (only when you use them): photos you take or pick are attached to the
  message you're writing and synced like any other attachment.
- **Sample data:** "Look around with sample data" on the sign-in screen runs entirely on the phone
  with made-up chats. Nothing is sent anywhere.
- The iPhone app doesn't send the usage ping, has no analytics, and doesn't track you across apps
  or websites.

### Deleting your data

Signing out on the iPhone deletes its local copy. To delete your account and everything
synced to our server, contact us (below) from the email you signed in with; we delete it within 30
days and confirm. You can also run your own sync server instead ([edge/DEPLOY.md](../edge/DEPLOY.md)),
in which case nothing reaches ours.

## The iPhone beta waitlist

The website's "Join the iPhone beta" form sends us your email address and, if you write one, a
short note. We use them only to decide who gets access during the beta and to send you the
TestFlight invite. They're stored on our server (separately from the usage pings), seen only by
the maintainers, and deleted when you ask or when the beta ends, whichever comes first. Your IP
address is used, in memory, only to rate-limit the form; it isn't stored.

## Contact

Questions, or a request to see or delete your data: message
[@ShriD5 on GitHub](https://github.com/ShriD5) or [@DevaiahShrithan on X](https://x.com/DevaiahShrithan),
or reply to your beta invite. Changes to this page are listed in the repository's history.
