# Going public: checklist

The repo went public on 2026-10-19 as ShriD5/godspeed-code.

## Done

- [x] Secret scan of all history (gitleaks, 2,626 commits): 37 hits, all test fixtures, variable
      names or redaction patterns, mostly from Zeron's own history. No real keys or tokens.
- [x] MIT license kept, Zeron credited in README and THIRD_PARTY_NOTICES.
- [x] Issue/PR templates, CONTRIBUTING, CODE_OF_CONDUCT, SECURITY, Dependabot.
- [x] Signed and notarized macOS releases.
- [x] README, roadmap and Zeron comparison current for 0.5.
- [x] Last user-visible Zeron branding removed (old logo svg, spinner word). Internal `zeron-*`
      crate names stay on purpose so upstream ports stay cheap.
- [x] CI green on the fork: Linux on every PR and push to main; macOS and Windows on manual
      dispatch or PRs labelled `full-ci` until the repo is public.

## Owner decisions (blockers)

1. **Name.** DECIDED 2026-10-19: public name **Godspeed Code** (repo `godspeed-code`); the app label stays "Godspeed"; use "Godspeed Code" as the App Store name.
   Background: "Godspeed – To Do List" (Otto Labs, LLC, godspeedapp.com) is an existing Mac/iOS
   productivity app. The App Store will not accept the same name for the iPhone app, and it is a
   trademark risk in the same category. Options: a variant ("Godspeed Code"), or rename before launch.
2. **Apple signing identity.** DECIDED 2026-10-19: stays under Devakumar N M's team.
   Background: Releases are signed as Devakumar N M (team QPUV967B5U). Agree on
   public use, or move signing to the owner's own developer account.
3. **Cloud sync on the owner's infrastructure.** Public users who choose Enable sync sign in through
   the WorkOS staging environment and use the owner's Cloudflare account. Before launch: WorkOS
   production environment, Workers Paid plan, rate limits; or ship with sign-in off
   (`GODSPEED_WORKOS_CLIENT_ID` empty by default) until a hosted cloud is ready.
4. **GitHub organisation** if collaborators need admin rights.

## Owner checks

- Eyeball pass on 0.5 (TEST-CHECKLIST §10, §12b, §15b, §15c).
- Optional: speedster frames restyled in gold to match the new icon.

## Launch day

- [x] Full CI triggers restored, repo public, links point at ShriD5/godspeed-code.
- Fresh release; launch video; posts (benchmark: 133 ms vs 877 ms start, 12 vs 166 MiB idle).
