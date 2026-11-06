# iPhone TestFlight beta

Godspeed for iPhone ships as a **public TestFlight beta** (one public link, through Apple's Beta
App Review). Cloud sync, which the app needs, stays **invite-only**: people join the waitlist on
the landing page and an admin approves them. Anyone with the TestFlight link can install the
app and look around with sample data; only approved emails can sign in to sync.

| Piece | Where |
| --- | --- |
| App record | App Store Connect, "Godspeed Code", bundle id `site.shrithan.godspeed.ios`, SKU `godspeed-ios`, team `QPUV967B5U` |
| Build + upload | `scripts/ios/testflight.sh` |
| Test info, review details, public group, submission | `scripts/ios/asc-setup.sh` (`scripts/ios/asc.py`) |
| Review demo account | `edge/scripts/review-account.mjs` → `~/.config/godspeed/testflight.env` |
| Waitlist form | https://godspeed-code.pages.dev/#iphone → edge `POST /waitlist` |
| Approvals | https://godspeed-edge.shrid5.workers.dev/admin/access |
| Privacy policy | https://godspeed-code.pages.dev/privacy (from `docs/PRIVACY.md`) |

## Owner checklist

Done once:

1. **Accept the Program License Agreement** (Devakumar, the Account Holder). Done 2026-10-30.
2. **Create the app record** in App Store Connect. The API can't create apps, so this is the
   one browser step: Apps → **+** → New App → iOS, Name **Godspeed Code** (if it's taken:
   **Godspeed for Coding Agents**), Primary language English (U.S.), Bundle ID
   `site.shrithan.godspeed.ios`, SKU `godspeed-ios`, User access Full Access.
3. **Review contact** for Beta App Review: add your details to `~/.config/godspeed/testflight.env`
   (it's 0600 and outside the repo), then `scripts/ios/asc-setup.sh beta-info`:

   ```sh
   REVIEW_CONTACT_FIRST_NAME=…
   REVIEW_CONTACT_LAST_NAME=…
   REVIEW_CONTACT_PHONE=+91…
   REVIEW_CONTACT_EMAIL=…
   FEEDBACK_EMAIL=…            # testers' TestFlight feedback goes here
   ```

   The same file already holds `REVIEW_DEMO_EMAIL` / `REVIEW_DEMO_PASSWORD`.
4. **Allow the demo account** on the edge: `/admin/access` → "Allow an email directly" →
   `app-review@godspeed-code.pages.dev`, or from a terminal
   `cd edge && node scripts/access.mjs allow app-review@godspeed-code.pages.dev` (skip if it's
   already listed under "Approved here").
5. **Run the pipeline** (from the repo root, on the Mac with the Distribution certificate):

   ```sh
   scripts/ios/asc-setup.sh all        # capabilities, test info, review details, public group
   scripts/ios/testflight.sh           # build, archive, upload (prints the build number)
   # wait for the processing email (5-30 min), then:
   scripts/ios/asc-setup.sh submit     # adds the build to "Public beta" and submits it for review
   scripts/ios/asc-setup.sh status     # review state and the public link
   ```

6. **Once Beta App Review approves**, put the public link in the edge's invite message:
   set `"TESTFLIGHT_URL"` in `edge/wrangler.jsonc` and deploy the edge. Copy it to the deploy
   directory first: `rsync -a edge/src edge/wrangler.jsonc /Volumes/Lexar/build/godspeed-edge-deploy/edge/`,
   then `cd /Volumes/Lexar/build/godspeed-edge-deploy/edge && npx wrangler deploy`.

Every later build: `scripts/ios/testflight.sh`, then `scripts/ios/asc-setup.sh submit`. Builds
after the first approved one usually skip the full review.

## Approving testers

1. Open https://godspeed-edge.shrid5.workers.dev/admin/access (admins sign in with WorkOS;
   admins are managed with `edge/scripts/admins.mjs`, see `edge/DEPLOY.md`). The dashboard links
   to it as **Beta access**.
2. Pending requests are at the top, oldest first, with the note each person left.
3. **Approve** adds the email to the cloud allow-list. It works within a minute and needs no
   redeploy. The page then shows the invite message: **Copy invite message** (paste into email or
   DM) or **Open in Mail**. The edge doesn't send email itself.
4. The invite tells them to install TestFlight from the public link, turn on sync on the Mac with
   the approved email, and sign in on the phone with the same email.
5. **Dismiss** hides a request; **Revoke** removes an approval; **Delete** erases an email
   entirely (privacy removal requests). **Allow an email directly** approves someone who isn't on
   the waitlist.
6. Without a browser: `cd edge && node scripts/access.mjs approve|allow|revoke|dismiss|forget <email>`
   signs a short admin session with the local WorkOS key and posts the same form.

Approvals add to the `ALLOWED_EMAILS` secret, which keeps working (and is the only place for
`@domain` entries). An approved person signs in with the exact email that was approved; if they
sign up with a different one, approve that one too.

## What the reviewer sees

The review notes (`review_notes()` in `scripts/ios/asc.py`) explain that the app is a companion
to the free Mac app, that agents run on the user's Mac, and how to review without one:

- **Look around with sample data** on the sign-in screen: the offline demo workspace (chats with
  streaming replies, Needs You, the Board, Usage). Nothing leaves the phone.
- **Demo account** `app-review@godspeed-code.pages.dev` (password in the review details): a real
  WorkOS email + password account in its own organization, allowed for sync. It has no Mac
  attached, so it signs in to an empty workspace, which the notes say is expected.

The demo account was created on 2026-10-30 by `edge/scripts/review-account.mjs` in the WorkOS
environment the edge uses (client `client_01M52B7VJ0PRXA4Y63HQKYYRA8`): user
`user_01M66QA9K9STD6AFMFQ9HH2DRX` (email marked verified) and organization
`org_01M66QAA6XP071YEB96B801E5X` ("Godspeed App Review"). To remove it after review: delete both
in WorkOS and the email on `/admin/access`.

## Answers for App Store Connect

- **Export compliance:** `ITSAppUsesNonExemptEncryption = false` in Info.plist, so uploads skip
  the question. The app uses only standard encryption for secure connections: HTTPS/WSS through
  rustls (ring) in the Rust core, and WebRTC's DTLS-SRTP for Codex voice calls. No proprietary
  or custom encryption, no end-to-end encryption feature.
- **Privacy:** no tracking. Collected (linked to the account, for app functionality): email,
  name, user id, device id, other user content (synced chats), photos (attachments). Matches
  `Godspeed/PrivacyInfo.xcprivacy` and `docs/PRIVACY.md`.
- **Background audio:** keeps a Codex voice call the user started alive while the screen is
  locked.
