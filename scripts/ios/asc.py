#!/usr/bin/env python3
"""App Store Connect setup for the Godspeed iPhone TestFlight beta.

Talks to the App Store Connect API with the team's API key. Run it through
scripts/ios/asc-setup.sh (which provides the Python deps), one step at a time
or all at once:

    scripts/ios/asc-setup.sh status        # app, builds, groups, review state
    scripts/ios/asc-setup.sh caps          # bundle ids + capabilities vs the entitlements
    scripts/ios/asc-setup.sh create-app    # checks for the app record; the API can't create one,
                                           # so it prints the browser steps ("Godspeed Code")
    scripts/ios/asc-setup.sh beta-info     # test information + beta app review details
    scripts/ios/asc-setup.sh group         # the public beta group and its public link
    scripts/ios/asc-setup.sh submit        # newest valid build -> group + beta app review
    scripts/ios/asc-setup.sh all           # caps, create-app, beta-info, group (not submit)

Secrets and personal details never live in the repo:
  - the API key: ASC_KEY_ID, ASC_ISSUER_ID, ASC_KEY_PATH in $GODSPEED_ASC_ENV
    (default ~/.config/outbound/keys.env; same team as Outbound);
  - review contact + demo account: $GODSPEED_TESTFLIGHT_ENV (default
    ~/.config/godspeed/testflight.env) with REVIEW_CONTACT_FIRST_NAME,
    REVIEW_CONTACT_LAST_NAME, REVIEW_CONTACT_PHONE, REVIEW_CONTACT_EMAIL,
    FEEDBACK_EMAIL, REVIEW_DEMO_EMAIL, REVIEW_DEMO_PASSWORD.
Nothing secret is printed.

Docs: docs/godspeed/TESTFLIGHT.md.
"""
import argparse
import os
import sys
import time

import jwt
import requests

API = "https://api.appstoreconnect.apple.com"
BUNDLE_ID = "site.shrithan.godspeed.ios"
EXT_BUNDLE_ID = BUNDLE_ID + ".LiveActivity"
APP_NAMES = ["Godspeed Code", "Godspeed for Coding Agents"]
SKU = "godspeed-ios"
PRIMARY_LOCALE = "en-US"
GROUP_NAME = "Public beta"
PUBLIC_LINK_LIMIT = 1000

# What the entitlements need (apps/ios/Godspeed.entitlements and
# GodspeedLiveActivity/GodspeedLiveActivity.entitlements). Live Activities
# need no capability, only NSSupportsLiveActivities in Info.plist. The App
# Group itself (group.site.shrithan.godspeed.ios) is created and assigned by
# Xcode's automatic signing during the archive (-allowProvisioningUpdates).
NEEDED_CAPABILITIES = {
    BUNDLE_ID: ("Godspeed", {"APP_GROUPS", "PUSH_NOTIFICATIONS"}),
    EXT_BUNDLE_ID: ("Godspeed LiveActivity", {"APP_GROUPS"}),
}

MARKETING_URL = "https://godspeed-code.pages.dev"
PRIVACY_URL = "https://godspeed-code.pages.dev/privacy"

BETA_DESCRIPTION = """Godspeed for iPhone is the companion to Godspeed Code, a free, open-source Mac app that runs coding agents (Claude Code, Codex, Cursor and more). The phone follows the chats running on your Mac: read transcripts as they stream, send messages, answer approvals in Needs You, watch every agent on the Board, check your plan usage, and dictate or talk to Codex by voice. Agents keep running on the Mac; the phone is a remote.

To connect it, install Godspeed Code on your Mac (godspeed-code.pages.dev), choose Enable sync in the account menu, and sign in with the same email on the phone. Cloud sync is invite-only during the beta: join the waitlist on the website and we'll approve your email.

No Mac handy? Tap "Look around with sample data" on the sign-in screen.

What to test: opening and following chats, sending messages and approvals, the Needs You inbox, notifications when a run finishes, voice dictation, and the usage widgets."""

WHATS_NEW = """First TestFlight build. Follow and drive your Mac's Godspeed chats from your phone: transcripts, composer, Needs You approvals, Mission Control board, usage and widgets, notifications, dictation and Codex voice. "Look around with sample data" on the sign-in screen shows everything without a Mac."""


def review_notes(demo_email: str) -> str:
    account = (
        f"""2. Demo account (cloud sync): sign in with {demo_email} and the password above. AuthKit (WorkOS) shows the sign-in page; enter the email, then the password. The account is approved for sync, but no Mac is attached to it, so the signed-in app is an empty workspace: that's expected, the content comes from the user's Mac. Use sample data (1) to see the app with content."""
        if demo_email
        else "2. Signing in needs a Godspeed Code account with sync turned on, from the Mac app; sample data (1) shows everything without one."
    )
    return f"""Godspeed for iPhone is a companion (remote control) to Godspeed Code, a free, open-source Mac app for running AI coding agents (github.com/ShriD5/godspeed-code). Agents run on the user's Mac; the phone shows their chats and sends messages and approvals to them through our sync server. The phone runs no agent and needs the user's own Mac.

How to review without a Mac:
1. On the sign-in screen, tap "Look around with sample data". This opens an offline demo workspace with made-up chats, streaming replies, the Needs You inbox, the Board and Usage. Nothing is sent anywhere. Settings > Leave Sample Data returns to sign-in.
{account}

Permissions: microphone and speech recognition for dictation and Codex voice calls (only when you tap the mic or start a call); camera and photos to attach images to a message. Background audio (UIBackgroundModes audio) keeps a voice call you started going when the screen locks; it is idle otherwise. Push notifications tell you when a chat on your Mac finishes or needs input.

Encryption: only standard HTTPS/TLS and WebRTC (DTLS-SRTP) for voice; ITSAppUsesNonExemptEncryption is false.

Cloud sync is invite-only (testers join a waitlist on godspeed-code.pages.dev and we approve them). The app is free, with no purchases or ads. Privacy policy: {PRIVACY_URL}"""


# ---------------------------------------------------------------------------
# Plumbing
# ---------------------------------------------------------------------------


def load_env(path: str, prefix: str = "") -> dict:
    out = {}
    path = os.path.expanduser(path)
    if not os.path.exists(path):
        return out
    for line in open(path, encoding="utf-8"):
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        k, v = line.split("=", 1)
        k = k.strip().removeprefix("export ").strip()
        if k.startswith(prefix):
            out[k] = v.strip().strip('"').strip("'")
    return out


KEYS = load_env(os.environ.get("GODSPEED_ASC_ENV", "~/.config/outbound/keys.env"), "ASC_")
TF = load_env(os.environ.get("GODSPEED_TESTFLIGHT_ENV", "~/.config/godspeed/testflight.env"))
_token = {"value": None, "exp": 0}


def token() -> str:
    if _token["value"] and time.time() < _token["exp"] - 60:
        return _token["value"]
    for k in ("ASC_KEY_ID", "ASC_ISSUER_ID", "ASC_KEY_PATH"):
        if not KEYS.get(k):
            sys.exit(f"missing {k} in the ASC env file")
    key = open(os.path.expanduser(KEYS["ASC_KEY_PATH"])).read()
    now = int(time.time())
    _token["value"] = jwt.encode(
        {"iss": KEYS["ASC_ISSUER_ID"], "iat": now, "exp": now + 1200, "aud": "appstoreconnect-v1"},
        key,
        algorithm="ES256",
        headers={"kid": KEYS["ASC_KEY_ID"], "typ": "JWT"},
    )
    _token["exp"] = now + 1200
    return _token["value"]


class AscError(Exception):
    def __init__(self, status, errors):
        self.status = status
        self.errors = errors or []
        detail = "; ".join(f"{e.get('code')}: {e.get('detail') or e.get('title')}" for e in self.errors) or str(status)
        super().__init__(f"HTTP {status}: {detail}")


def call(method: str, path: str, body=None, params=None):
    r = requests.request(
        method,
        API + path,
        headers={"Authorization": "Bearer " + token(), "Content-Type": "application/json"},
        json=body,
        params=params,
        timeout=60,
    )
    if r.status_code == 204:
        return None
    data = r.json() if r.content else {}
    if r.status_code >= 400:
        raise AscError(r.status_code, data.get("errors"))
    return data


def get(path, **params):
    return call("GET", path, params=params or None)


def post(path, body):
    return call("POST", path, body)


def patch(path, body):
    return call("PATCH", path, body)


def rel(type_, id_):
    return {"data": {"type": type_, "id": id_}}


# ---------------------------------------------------------------------------
# Steps
# ---------------------------------------------------------------------------


def find_bundle_id(identifier: str):
    data = get("/v1/bundleIds", **{"filter[identifier]": identifier, "limit": 20})["data"]
    return next((d for d in data if d["attributes"]["identifier"] == identifier), None)


def find_app():
    data = get("/v1/apps", **{"filter[bundleId]": BUNDLE_ID})["data"]
    return next((d for d in data if d["attributes"]["bundleId"] == BUNDLE_ID), None)


def step_caps():
    for identifier, (name, needed) in NEEDED_CAPABILITIES.items():
        bid = find_bundle_id(identifier)
        if not bid:
            bid = post(
                "/v1/bundleIds",
                {"data": {"type": "bundleIds", "attributes": {"identifier": identifier, "name": name, "platform": "IOS"}}},
            )["data"]
            print(f"registered bundle id {identifier} ({bid['id']})")
        have = {c["attributes"]["capabilityType"] for c in get(f"/v1/bundleIds/{bid['id']}/bundleIdCapabilities")["data"]}
        for cap in sorted(needed - have):
            post(
                "/v1/bundleIdCapabilities",
                {
                    "data": {
                        "type": "bundleIdCapabilities",
                        "attributes": {"capabilityType": cap},
                        "relationships": {"bundleId": rel("bundleIds", bid["id"])},
                    }
                },
            )
            print(f"enabled {cap} on {identifier}")
        missing = needed - have
        print(f"{identifier} ({bid['id']}): {', '.join(sorted(have | needed))}{' (added ' + ', '.join(sorted(missing)) + ')' if missing else ' (ok)'}")


def step_create_app():
    app = find_app()
    if app:
        print(f"app record exists: {app['attributes']['name']} ({app['id']})")
        return app
    bid = find_bundle_id(BUNDLE_ID)
    if not bid:
        sys.exit("bundle id missing; run caps first")
    last = None
    for name in APP_NAMES:
        try:
            app = post(
                "/v1/apps",
                {
                    "data": {
                        "type": "apps",
                        "attributes": {"name": name, "primaryLocale": PRIMARY_LOCALE, "sku": SKU, "bundleId": BUNDLE_ID},
                        "relationships": {"bundleId": rel("bundleIds", bid["id"])},
                    }
                },
            )["data"]
            print(f"created app record: {name} ({app['id']})")
            return app
        except AscError as e:
            last = e
            if e.status == 403 and any("does not allow 'CREATE'" in (x.get("detail") or "") for x in e.errors):
                # The App Store Connect API can read and update apps but not
                # create them: this one step is in the browser.
                sys.exit(
                    "The App Store Connect API can't create app records. Create it in the browser:\n"
                    "  appstoreconnect.apple.com > Apps > + > New App\n"
                    f"  Platform iOS, Name '{APP_NAMES[0]}' (if taken: '{APP_NAMES[1]}'), Primary language English (U.S.),\n"
                    f"  Bundle ID {BUNDLE_ID}, SKU {SKU}, User access Full Access.\n"
                    "Then run: scripts/ios/asc-setup.sh all"
                )
            print(f"could not create '{name}': {e}")
            taken = any("name" in (x.get("detail") or "").lower() or x.get("code", "").endswith("DUPLICATE") for x in e.errors)
            if not taken and e.status != 409:
                break
    sys.exit(f"app record not created: {last}")


def need_app():
    app = find_app()
    if not app:
        sys.exit("no app record for site.shrithan.godspeed.ios yet; run create-app (or create it in App Store Connect)")
    return app


def step_beta_info():
    app = need_app()
    app_id = app["id"]
    feedback = TF.get("FEEDBACK_EMAIL") or TF.get("REVIEW_CONTACT_EMAIL")
    attrs = {
        "description": BETA_DESCRIPTION,
        "marketingUrl": MARKETING_URL,
        "privacyPolicyUrl": PRIVACY_URL,
    }
    if feedback:
        attrs["feedbackEmail"] = feedback
    locs = get(f"/v1/apps/{app_id}/betaAppLocalizations")["data"]
    loc = next((l for l in locs if l["attributes"]["locale"] == PRIMARY_LOCALE), None)
    if loc:
        patch(f"/v1/betaAppLocalizations/{loc['id']}", {"data": {"type": "betaAppLocalizations", "id": loc["id"], "attributes": attrs}})
    else:
        post(
            "/v1/betaAppLocalizations",
            {"data": {"type": "betaAppLocalizations", "attributes": {"locale": PRIMARY_LOCALE, **attrs}, "relationships": {"app": rel("apps", app_id)}}},
        )
    print(f"test information set (feedback email {'set' if feedback else 'MISSING: add FEEDBACK_EMAIL'})")

    demo_email = TF.get("REVIEW_DEMO_EMAIL", "")
    detail = get(f"/v1/apps/{app_id}/betaAppReviewDetail")["data"]
    review = {
        "notes": review_notes(demo_email),
        "demoAccountRequired": bool(demo_email),
    }
    if demo_email:
        review["demoAccountName"] = demo_email
        review["demoAccountPassword"] = TF.get("REVIEW_DEMO_PASSWORD", "")
    for env_key, field in (
        ("REVIEW_CONTACT_FIRST_NAME", "contactFirstName"),
        ("REVIEW_CONTACT_LAST_NAME", "contactLastName"),
        ("REVIEW_CONTACT_PHONE", "contactPhone"),
        ("REVIEW_CONTACT_EMAIL", "contactEmail"),
    ):
        if TF.get(env_key):
            review[field] = TF[env_key]
    patch(f"/v1/betaAppReviewDetails/{detail['id']}", {"data": {"type": "betaAppReviewDetails", "id": detail["id"], "attributes": review}})
    missing = [f for f in ("contactFirstName", "contactLastName", "contactPhone", "contactEmail") if f not in review]
    print(f"beta review details set (demo account {'set' if demo_email else 'none'}; contact {'complete' if not missing else 'MISSING ' + ', '.join(missing)})")


def find_group(app_id):
    groups = get(f"/v1/apps/{app_id}/betaGroups")["data"]
    return next((g for g in groups if g["attributes"]["name"] == GROUP_NAME), None)


def step_group():
    app = need_app()
    group = find_group(app["id"])
    attrs = {"publicLinkEnabled": True, "publicLinkLimitEnabled": True, "publicLinkLimit": PUBLIC_LINK_LIMIT, "feedbackEnabled": True}
    if not group:
        group = post(
            "/v1/betaGroups",
            {"data": {"type": "betaGroups", "attributes": {"name": GROUP_NAME, **attrs}, "relationships": {"app": rel("apps", app["id"])}}},
        )["data"]
        print(f"created group '{GROUP_NAME}' ({group['id']})")
    else:
        group = patch(f"/v1/betaGroups/{group['id']}", {"data": {"type": "betaGroups", "id": group["id"], "attributes": attrs}})["data"]
    link = group["attributes"].get("publicLink")
    print(f"public link: {link or '(appears once a build is in the group and approved)'}")
    return group


def newest_build(app_id):
    builds = get("/v1/builds", **{"filter[app]": app_id, "sort": "-uploadedDate", "limit": 10, "include": "preReleaseVersion"})
    return builds["data"]


def step_submit(build_number=None):
    app = need_app()
    group = find_group(app["id"]) or step_group()
    builds = newest_build(app["id"])
    if build_number:
        builds = [b for b in builds if b["attributes"]["version"] == build_number]
    if not builds:
        sys.exit("no build uploaded yet (scripts/ios/testflight.sh)")
    build = builds[0]
    a = build["attributes"]
    print(f"build {a['version']}: processing {a['processingState']}")
    if a["processingState"] != "VALID":
        sys.exit("build still processing; try again in a few minutes")
    # What to test, shown to testers.
    locs = get(f"/v1/builds/{build['id']}/betaBuildLocalizations")["data"]
    loc = next((l for l in locs if l["attributes"]["locale"] == PRIMARY_LOCALE), None)
    if loc:
        patch(f"/v1/betaBuildLocalizations/{loc['id']}", {"data": {"type": "betaBuildLocalizations", "id": loc["id"], "attributes": {"whatsNew": WHATS_NEW}}})
    else:
        post(
            "/v1/betaBuildLocalizations",
            {"data": {"type": "betaBuildLocalizations", "attributes": {"locale": PRIMARY_LOCALE, "whatsNew": WHATS_NEW}, "relationships": {"build": rel("builds", build["id"])}}},
        )
    post(f"/v1/betaGroups/{group['id']}/relationships/builds", {"data": [{"type": "builds", "id": build["id"]}]})
    print(f"build {a['version']} added to '{GROUP_NAME}'")
    try:
        post("/v1/betaAppReviewSubmissions", {"data": {"type": "betaAppReviewSubmissions", "relationships": {"build": rel("builds", build["id"])}}})
        print("submitted for beta app review")
    except AscError as e:
        if e.status == 409:
            print(f"not submitted: {e}")
        else:
            raise


def step_status():
    for identifier in (BUNDLE_ID, EXT_BUNDLE_ID):
        bid = find_bundle_id(identifier)
        print(f"bundle id {identifier}: {bid['id'] if bid else 'MISSING'}")
    app = find_app()
    if not app:
        print("app record: none")
        return
    print(f"app record: {app['attributes']['name']} ({app['id']})")
    for b in newest_build(app["id"])[:5]:
        a = b["attributes"]
        review = get(f"/v1/builds/{b['id']}/betaAppReviewSubmission").get("data")
        state = review["attributes"]["betaReviewState"] if review else "not submitted"
        print(f"  build {a['version']} uploaded {a['uploadedDate'][:16]} processing={a['processingState']} review={state}")
    group = find_group(app["id"])
    if group:
        g = group["attributes"]
        print(f"group '{GROUP_NAME}': public link {g.get('publicLink') or '(none yet)'} enabled={g.get('publicLinkEnabled')}")
    else:
        print(f"group '{GROUP_NAME}': none")


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("step", choices=["status", "caps", "create-app", "beta-info", "group", "submit", "all"])
    p.add_argument("--build", help="build number (CFBundleVersion) to submit; default newest")
    args = p.parse_args()
    try:
        if args.step == "status":
            step_status()
        elif args.step == "caps":
            step_caps()
        elif args.step == "create-app":
            step_create_app()
        elif args.step == "beta-info":
            step_beta_info()
        elif args.step == "group":
            step_group()
        elif args.step == "submit":
            step_submit(args.build)
        else:
            step_caps()
            step_create_app()
            step_beta_info()
            step_group()
    except AscError as e:
        sys.exit(f"App Store Connect: {e}")


if __name__ == "__main__":
    main()
