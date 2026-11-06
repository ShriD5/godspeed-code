# Contributing to Godspeed

Thanks for helping. Godspeed is in early access and maintained by one person, so small, focused
pull requests are the easiest to land.

## Before you start

- For anything larger than a focused fix, open an issue first so the approach can be agreed before
  you invest in it.
- Keep a pull request to one concern. A bug fix, a refactor and a new feature are three PRs.
- Read [ARCHITECTURE.md](ARCHITECTURE.md) for the crate map and data model (inherited from Zeron),
  [CONTEXT.md](CONTEXT.md) for vocabulary, and [docs/godspeed/](docs/godspeed/) for Godspeed's
  roadmap and plans. `docs/` holds design notes for most subsystems; check there before redesigning
  something.
- Internal crates keep their `zeron-*` names on purpose, so fixes from upstream
  [Zeron](https://github.com/zeronsh/zeron) can still be cherry-picked. Don't rename them.

### Where to start

- Open issues, especially ones labelled `good first issue`.
- The unchecked items in [ROADMAP.md](docs/godspeed/ROADMAP.md).
- Harness bugs: each agent driver lives in its own folder under `crates/harness/src/`.

## Setup

The toolchain is pinned by [`rust-toolchain.toml`](rust-toolchain.toml) (stable, with `rustfmt`
and `clippy`). macOS needs Xcode. Linux needs the GPUI system libraries:

```sh
sudo apt-get install -y libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev \
  libx11-dev libxcb1-dev libx11-xcb-dev libfontconfig1-dev libfreetype-dev \
  libasound2-dev libvulkan-dev pkg-config cmake libwebkit2gtk-4.1-dev libjson-glib-dev
```

Windows is covered in [docs/reference/windows-development.md](docs/reference/windows-development.md).

A clean build takes a while (about 20 minutes on an M2 Air); incremental builds are much faster.
On a machine with limited memory, cap parallelism with `CARGO_BUILD_JOBS=4`.

```sh
cargo build -p godspeed
cargo run -p godspeed               # desktop app
cargo run -p godspeed -- headless   # engine only
```

### Building with several agents at once

If you run several Godspeed agents on this repo at once (one worktree each), every `cargo build`
starts one rustc per core by default, and the memory adds up fast: four builds on a 16 GB laptop is
enough to make the machine swap. To keep it usable:

- Leave **Settings → General → Builds → Share build slots across agents** on (it is on by default).
  Godspeed runs one GNU-make-style jobserver and points every agent and terminal it starts at it
  through `CARGO_MAKEFLAGS`, so all the builds share one pool of compile jobs. A build running
  alone gets the same parallelism as before. If you set `CARGO_BUILD_JOBS`, `MAKEFLAGS` or
  `build.jobs` yourself, your setting wins and the pool stays out of the way.
- Install sccache (`brew install sccache`) and Godspeed sets `RUSTC_WRAPPER` for agents in Rust
  projects, so worktrees share compiled dependencies instead of building them again.
- Point worktrees at one shared `CARGO_TARGET_DIR` only if they're on the same branch. Otherwise
  give each worktree its own target directory, or the builds keep invalidating each other's work.
- The dev profile builds dependencies without debug info (`[profile.dev.package."*"]
  debug = false`). Workspace crates keep full debug info. If you need to step into a dependency
  in a debugger, override it locally for that crate.

The macOS system `make` (3.81) can't read the pool's FIFO. If a build script pipes
`CARGO_MAKEFLAGS` into `make` (the `cmake`, `openssl-src` and `jemalloc-sys` crates do), that
`make` fails. Godspeed only shares the pool with Rust projects whose `Cargo.lock` has none of
those crates, unless GNU make 4.4 or newer is first on `PATH`. Godspeed's own lockfile is
eligible.

### Running a dev build next to an installed Godspeed

The installed app holds the default data directory (`~/.godspeed`) and IPC port (27654). Give your
dev build its own so the two never share state:

```sh
GODSPEED_DATA_DIR=~/.godspeed-dev GODSPEED_IPC_PORT=27700 cargo run -p godspeed
```

Useful variables for exercising the UI without a real agent or account:

| Variable | Effect |
| --- | --- |
| `GODSPEED_HARNESS=mock` | Offers the mock harness, which streams canned turns |
| `GODSPEED_MOCK_SUBAGENT=1` | Mock turns spawn subagents |
| `GODSPEED_MOCK_THINKING=1` | Mock turns include markdown-heavy thinking |
| `GODSPEED_MOCK_TODO=1` | Mock turns work through an 8-item checklist (pair with `GODSPEED_MOCK_DELAY_MS=900`) |

Logs from the desktop app go to `~/.godspeed/logs/godspeed-headed.log` (the previous launch is kept
as `.log.old`); the headless engine writes `godspeed-headless.log` in the same folder.

## Tests

GitHub Actions runs the Linux suites on every pull request. While the repository is private, the
macOS and Windows jobs only run when the PR has the `full-ci` label (or on a manual dispatch), so
for platform-specific changes add the label or run the local tests below. Say in the PR what you
ran.

```sh
cargo build -p godspeed
cargo check --workspace --tests

cargo test -p zeron-engine --lib
cargo test -p zeron-engine --test e2e
cargo test -p zeron-engine --test always_on_agents
cargo test -p zeron-engine --test git_actions
cargo test -p zeron-engine --test chat_import

cargo test -p zeron-harness --lib
cargo test -p zeron-harness --test claude
cargo test -p zeron-harness --test codex
cargo test -p zeron-harness --test opencode
cargo test -p zeron-harness --test acp

cargo test -p zeron-ui --lib -- --test-threads=1   # can abort when run fully in parallel
```

Run the suites for the crates you touched. `cargo test -p zeron-harness --test muse_live` needs
`muse` installed and uses its free echo provider.

- Every bug fix comes with a test that fails without the fix.
- Tests must be deterministic and offline: no real network, no real agent CLIs, no reliance on
  your home directory or your real `~/.claude`, `~/.codex` or `~/.godspeed` data. Use fixtures
  under `crates/*/tests/` and temp dirs.
- Two tests are known to flake under heavy load and pass alone:
  `version_probe_bounds_hangs_and_rejects_nonzero` and
  `runtime_survival::a_stop_right_after_send_stops_that_turn`.

## Code

- Match the surrounding code: naming, idioms and comment density. Comments explain why, not what.
- `cargo clippy --all-targets` should add no new warnings in code you touched.
- Format only your own changes. Parts of the tree are not rustfmt-clean, and whole-file reformatting
  buries the real diff.
- Don't add a dependency without saying why in the PR.
- Don't bump the version. It lives only in `[workspace.package]` in `Cargo.toml` and is changed at
  release time.
- New fields on wire and document types (`zeron-proto`, `zeron-doc`) must be optional or
  `#[serde(default)]`, because a UI may talk to an older engine. Never rename persisted keys, RPC
  method names or document fields; add new ones.
- New agent integrations follow the existing drivers in `crates/harness` and must drive agent CLIs
  only in ways their terms allow. Installs are explicit user actions
  ([crates/harness/README.md](crates/harness/README.md)).

## UI changes

- Reuse the app's existing rows, menus, hover treatments and empty states. If something similar
  exists, copy it.
- Do what the change calls for and nothing more.
- Check light and dark mode, frosted and opaque surfaces.
- Animations use the shared motion kit (`crate::motion`).

## Commits and pull requests

Commits use [Conventional Commits](https://www.conventionalcommits.org/) with an imperative summary:

```
feat(ui): add the approval card and access-mode picker
fix(harness): cap a single JSON-RPC line from an agent
docs: add headless benchmarks against T3 Code
```

Common types: `feat`, `fix`, `docs`, `test`, `refactor`, `perf`, `chore`. The scope is optional and
usually names the crate or area.

A pull request should have:

- A conventional-commit title.
- A short summary of what changed and why, calling out anything reviewers should look at closely
  (compatibility, migrations, behavior changed on purpose).
- The tests you added and the suites you ran.
- Screenshots or a short recording for every visible change, before and after for existing UI.
  Attach them to the PR; don't commit them. Redact emails, tokens, account names and home paths.

Keep the branch mergeable with `main`, and respond to review with new commits rather than
force-pushing over history the reviewer already read.

## Security

Never commit secrets, tokens or real account data, including in fixtures, screenshots and logs.
Report vulnerabilities privately as described in [SECURITY.md](SECURITY.md), not in a public issue.

## License

Godspeed is [MIT licensed](LICENSE). By contributing, you agree that your contributions are licensed
under the same terms.
