# Deploying the Godspeed edge

The edge is one Cloudflare Worker (`godspeed-edge`) with five SQLite-backed
Durable Object classes and one R2 bucket. It runs on the Workers free plan
(SQLite-backed Durable Objects are included there). Config: `wrangler.jsonc`,
account `f715952f0d5650529fe9401bcca31e2e`, served on workers.dev only.

## What it needs

| Kind | Name | Needed for | Value |
| --- | --- | --- | --- |
| R2 bucket | `godspeed-blobs` (binding `BLOBS`) | tool output, attachments, checkpoints | create once (step 3) |
| var | `AUTH_MODE` | auth | `workos` (never `dev` in production: `dev` accepts any bearer as the user id) |
| var | `WORKOS_CLIENT_ID` | verifying sign-in JWTs | `unset` until sign-in is chosen; every request is rejected until then |
| var | `APNS_TEAM_ID` | push | `QPUV967B5U` |
| var | `APNS_TOPIC` | push | `site.shrithan.godspeed.ios` (the iOS bundle id) |
| var (optional) | `WORKOS_ISSUER`, `WORKOS_JWKS_URL` | custom WorkOS trust anchor | default derived from the client id |
| secret | `WORKOS_API_KEY` | `/auth/exchange`, `/auth/refresh`, `/auth/orgs` (WorkOS sign-in) | WorkOS secret key; unset ⇒ those routes answer 501 |
| secret | `APNS_KEY_P8` | push notifications | contents of `AuthKey_XXXXXXXXXX.p8` |
| secret | `APNS_KEY_ID` | push notifications | the key's 10-character id |
| secret | `ALLOWED_EMAILS` | invite-only sign-in (optional) | comma-separated emails and `@domain` entries; unset ⇒ anyone who can sign in may sync |

Push is optional: without the two APNs secrets the edge decides and logs
notifications (`/registry/:org/stats`) but sends none.

## Steps, in order

```sh
cd edge
npm ci

# 1. Log in and confirm the account (expects f715952f0d5650529fe9401bcca31e2e).
npx wrangler login
npx wrangler whoami

# 2. R2 must be enabled on the account once (dashboard → R2 → enable; the
#    free tier is 10 GB-month, Cloudflare may ask for a payment method).

# 3. Create the bucket.
npx wrangler r2 bucket create godspeed-blobs

# 4. Sign-in config. If WorkOS: set "WORKOS_CLIENT_ID" in wrangler.jsonc to the
#    client id (client_…), and in the WorkOS dashboard add the redirect URI
#    godspeed://callback. (A pairing-code scheme would replace this step.)

# 5. Deploy. Prints https://godspeed-edge.<subdomain>.workers.dev
npx wrangler deploy

# 6. Secrets (each prompts for the value and rolls out a new version).
npx wrangler secret put WORKOS_API_KEY          # only with WorkOS sign-in
npx wrangler secret put APNS_KEY_P8 < AuthKey_XXXXXXXXXX.p8
npx wrangler secret put APNS_KEY_ID
npx wrangler secret put ALLOWED_EMAILS          # only for an invite-only edge

# 7. Check it.
curl https://godspeed-edge.<subdomain>.workers.dev/health   # {"ok":true,"auth":"workos"}
```

Then point the apps at it: the desktop reads `GODSPEED_EDGE_URL` and
`GODSPEED_WORKOS_CLIENT_ID` (`apps/godspeed/src/main.rs`; the baked defaults are
in `crates/client/src/auth.rs`), the iOS app uses `Endpoints` in
`apps/ios/Godspeed/Shell/SignInViewController.swift` (Debug builds also take
`-edge <url>`).

## Removed from Zeron's config, and why

- **Custom-domain routes** (`edge.zeron.sh`, `zeron.sh/install.sh`,
  `zeron.sh/releases/*` and the `comet.zeron.sh` legacy hosts): they live on
  Zeron's zone and would fail to deploy on this account. workers.dev instead.
- **`RELEASES` bucket** (`/releases/*`, `/install.sh`): Godspeed ships
  releases on GitHub. `src/install.sh` is rebranded for Godspeed but its
  default `GODSPEED_BASE_URL` is a `godspeed.invalid` placeholder. The binding
  is now optional and both routes answer 404 without it. To bring the curl
  installer back, set the real base URL in `install.sh`, create a
  `godspeed-releases` bucket and add the binding.
- **Zeron's WorkOS client id**: it would make this edge accept any Zeron
  account. Replaced by the `unset` placeholder (fails closed).
- **`wrangler.chat2test.jsonc`**: a temporary dev-auth deployment on Zeron's
  account. `wrangler.transport-test.jsonc` stays for
  `docs/transport-reliability.md`, renamed to `godspeed-transport-test`; it
  also runs dev auth, so delete it right after a test run.
- The worker was `comet-native-edge` (kept for Zeron's existing Durable Object
  data). This is a fresh deployment, so it is `godspeed-edge`; the Durable
  Object classes and migration tags `v1`–`v4` are unchanged.

## Free-plan limits worth knowing

Durable Objects on the free plan: 100,000 requests and 13,000 GB-s of duration
per day; Worker bundle 3 MB compressed (this one is about 1.5 MB). Heavy
always-on sync from several machines could hit the daily request cap; Workers
Paid lifts it.
