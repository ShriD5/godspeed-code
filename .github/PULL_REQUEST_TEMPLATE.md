## Summary

<!-- What changed and why, in a few bullets. Call out compatibility, migrations or behavior changed on purpose. -->

-

## Tests

<!-- Tests you added and the suites you ran. CI is not enabled yet, so this list is the gate. -->

- [ ] `cargo build -p godspeed`
- [ ] `cargo check --workspace --tests`
- [ ] Crate suites:

## Screenshots

<!-- Required for any visible change: before and after, light and dark if colors or surfaces changed. Attach images here; don't commit them. Redact emails, tokens and home paths. -->

## Checklist

- [ ] The PR title follows Conventional Commits (`feat:`, `fix:`, `docs:` ...).
- [ ] One concern per PR.
- [ ] Bug fixes include a test that fails without the fix.
- [ ] New wire or document fields are optional or `#[serde(default)]`.
- [ ] No secrets, tokens or personal data in code, fixtures, screenshots or logs.
- [ ] No version bump.
